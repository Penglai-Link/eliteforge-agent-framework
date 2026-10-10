//! Optional managed SDK runtime. Explicit-provider TelemetryPlugin users keep ownership.
mod process;
use eliteforge_agent_core::{Plugin, PluginContext, PluginError};
use eliteforge_agent_official_plugin_otel_collector::CollectorService;
use opentelemetry::KeyValue;
use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::{
    Resource,
    metrics::{PeriodicReader, SdkMeterProvider},
    trace::SdkTracerProvider,
};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;
#[derive(Clone)]
pub struct OtelRuntimeConfig {
    pub resource: BTreeMap<String, String>,
    /// None resolves the endpoint from the Collector plugin's typed service.
    pub endpoint: Option<String>,
    pub headers: HashMap<String, String>,
    pub export_interval: Duration,
    pub export_timeout: Duration,
    pub process_metrics: bool,
    pub use_proxy: bool,
}
impl Default for OtelRuntimeConfig {
    fn default() -> Self {
        Self {
            resource: BTreeMap::new(),
            endpoint: None,
            headers: HashMap::new(),
            export_interval: Duration::from_secs(15),
            export_timeout: Duration::from_secs(5),
            process_metrics: false,
            use_proxy: true,
        }
    }
}
#[derive(Clone)]
pub struct OtelRuntimeService {
    pub traces: SdkTracerProvider,
    pub metrics: SdkMeterProvider,
    pub instance_id: String,
}
impl OtelRuntimeService {
    pub async fn force_flush(&self) -> Result<(), String> {
        let traces = self.traces.clone();
        let metrics = self.metrics.clone();
        tokio::task::spawn_blocking(move || {
            let a = traces.force_flush().map_err(|e| e.to_string());
            let b = metrics.force_flush().map_err(|e| e.to_string());
            a.and(b)
        })
        .await
        .map_err(|e| e.to_string())?
    }
    fn shutdown(&self) -> Result<(), String> {
        let a = self.traces.shutdown().map_err(|e| e.to_string());
        let b = self.metrics.shutdown().map_err(|e| e.to_string());
        a.and(b)
    }
}
pub struct OtelRuntimePlugin {
    pub config: OtelRuntimeConfig,
}
#[async_trait::async_trait]
impl Plugin for OtelRuntimePlugin {
    fn id(&self) -> &str {
        "otel.runtime"
    }
    fn dependencies(&self) -> Vec<String> {
        if self.config.endpoint.is_none() {
            vec!["otel.collector".into()]
        } else {
            vec![]
        }
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        if self.config.export_interval.is_zero() || self.config.export_timeout.is_zero() {
            return Err(PluginError::Message(
                "invalid OTel export interval/timeout".into(),
            ));
        }
        let mut config = self.config.clone();
        if config.endpoint.is_none() {
            let collector = context
                .services()
                .get::<CollectorService>()
                .ok_or_else(|| PluginError::Message("collector endpoint required".into()))?;
            config.endpoint = Some(collector.endpoint.clone());
            config
                .headers
                .insert("authorization".into(), collector.authorization.clone());
        }
        let instance_id = config
            .resource
            .entry("service.instance.id".into())
            .or_insert_with(|| {
                format!(
                    "{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos()
                )
            })
            .clone();
        config
            .resource
            .insert("process.pid".into(), std::process::id().to_string());
        let service = tokio::task::spawn_blocking(move || -> Result<_, String> {
            let resource = Resource::builder_empty()
                .with_attributes(
                    config
                        .resource
                        .into_iter()
                        .map(|(k, v)| KeyValue::new(k, v)),
                )
                .build();
            let client = if config.use_proxy {
                reqwest::blocking::Client::builder()
            } else {
                reqwest::blocking::Client::builder().no_proxy()
            }
            .build()
            .map_err(|e| e.to_string())?;
            let endpoint = config.endpoint.unwrap().trim_end_matches('/').to_owned();
            let spans = opentelemetry_otlp::SpanExporter::builder()
                .with_http()
                .with_http_client(client.clone())
                .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
                .with_endpoint(format!("{endpoint}/v1/traces"))
                .with_headers(config.headers.clone())
                .with_timeout(config.export_timeout)
                .build()
                .map_err(|e| e.to_string())?;
            let metrics = opentelemetry_otlp::MetricExporter::builder()
                .with_http()
                .with_http_client(client)
                .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
                .with_endpoint(format!("{endpoint}/v1/metrics"))
                .with_headers(config.headers)
                .with_timeout(config.export_timeout)
                .build()
                .map_err(|e| e.to_string())?;
            Ok(OtelRuntimeService {
                traces: SdkTracerProvider::builder()
                    .with_resource(resource.clone())
                    .with_batch_exporter(spans)
                    .build(),
                metrics: SdkMeterProvider::builder()
                    .with_resource(resource)
                    .with_reader(
                        PeriodicReader::builder(metrics)
                            .with_interval(config.export_interval)
                            .build(),
                    )
                    .build(),
                instance_id,
            })
        })
        .await
        .map_err(|e| PluginError::Message(e.to_string()))?
        .map_err(PluginError::Message)?;
        let stop = CancellationToken::new();
        let shutdown = Arc::new(AtomicBool::new(false));
        let fallback = stop.clone();
        let fallback_service = service.clone();
        let fallback_shutdown = shutdown.clone();
        context.on_cleanup(move || {
            fallback.cancel();
            if !fallback_shutdown.swap(true, Ordering::AcqRel) {
                std::thread::spawn(move || {
                    let _ = fallback_service.shutdown();
                });
            }
        });
        let sampler = self.config.process_metrics.then(|| {
            process::start(
                service.metrics.clone(),
                stop.clone(),
                self.config.export_interval,
            )
        });
        let close = service.clone();
        context.on_shutdown(move || async move {
            stop.cancel();
            if let Some(sampler) = sampler {
                let _ = sampler.await;
            }
            if shutdown.swap(true, Ordering::AcqRel) {
                return Ok(());
            }
            tokio::task::spawn_blocking(move || close.shutdown())
                .await
                .map_err(|e| e.to_string())?
        });
        context.provide(service)
    }
}
