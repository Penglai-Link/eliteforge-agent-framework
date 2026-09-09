//! A separate test process isolates mutations of OpenTelemetry's global providers.
use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_pack::{
    EliteForgeOfficialPluginPack,
    telemetry::{TelemetryConfig, TelemetryPlugin},
};
use opentelemetry::{
    global,
    trace::{Span, Tracer, TracerProvider},
};
use opentelemetry_sdk::{
    metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider},
    trace::{InMemorySpanExporter, SdkTracerProvider},
};

struct Reply;
#[async_trait]
impl ChatModel for Reply {
    async fn generate(
        &self,
        _: &str,
        _: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        Ok(ChatResponse::text("done"))
    }
}

#[tokio::test]
async fn default_pack_resolves_global_providers_at_build_and_explicit_telemetry_replaces_them() {
    let directory = tempfile::tempdir().unwrap();
    // Providers are configured after the pack has been added to the builder.
    let builder = AgentBuilder::new().plugin_pack(
        EliteForgeOfficialPluginPack::new(directory.path().join("global.sqlite"))
            .model(ChatModelPlugin::new(Reply)),
    );
    let exporter = InMemorySpanExporter::default();
    let traces = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let metric_exporter = InMemoryMetricExporter::default();
    let metrics = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(metric_exporter.clone()).build())
        .build();
    global::set_tracer_provider(traces.clone());
    global::set_meter_provider(metrics.clone());

    let agent = builder.build().await.unwrap();
    assert_eq!(
        agent
            .run(RunRequest::new("chat", "hello"))
            .await
            .unwrap()
            .output,
        "done"
    );
    assert!(agent.close().await.is_success());
    traces.force_flush().unwrap();
    metrics.force_flush().unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    assert_eq!(
        spans.iter().filter(|span| span.name == "agent.run").count(),
        1
    );
    assert_eq!(
        spans
            .iter()
            .filter(|span| span.name == "agent.model")
            .count(),
        1
    );
    assert!(
        format!("{:?}", metric_exporter.get_finished_metrics().unwrap())
            .contains("agent.operation.count")
    );

    let explicit_exporter = InMemorySpanExporter::default();
    let explicit_traces = SdkTracerProvider::builder()
        .with_simple_exporter(explicit_exporter.clone())
        .build();
    let agent = AgentBuilder::new()
        .plugin_pack(
            EliteForgeOfficialPluginPack::new(directory.path().join("explicit.sqlite"))
                .model(ChatModelPlugin::new(Reply))
                .telemetry(
                    TelemetryPlugin::new(
                        "explicit.telemetry",
                        &explicit_traces,
                        &metrics,
                        TelemetryConfig::default(),
                    )
                    .unwrap(),
                ),
        )
        .build()
        .await
        .unwrap();
    agent
        .run(RunRequest::new("chat", "hello again"))
        .await
        .unwrap();
    assert!(agent.close().await.is_success());
    explicit_traces.force_flush().unwrap();
    assert_eq!(
        explicit_exporter
            .get_finished_spans()
            .unwrap()
            .iter()
            .filter(|span| span.name == "agent.run")
            .count(),
        1
    );
    assert_eq!(
        exporter.get_finished_spans().unwrap().len(),
        spans.len(),
        "explicit providers must replace default telemetry, not duplicate it"
    );

    // Agent shutdown keeps the host's providers usable.
    traces.tracer("host").start("after-agent-close").end();
    traces.force_flush().unwrap();
    assert!(
        exporter
            .get_finished_spans()
            .unwrap()
            .iter()
            .any(|span| span.name == "after-agent-close")
    );
    explicit_traces.shutdown().unwrap();
    traces.shutdown().unwrap();
    metrics.shutdown().unwrap();
}
