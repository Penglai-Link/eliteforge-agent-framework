#![cfg(feature = "runtime")]
use eliteforge_agent_core::*;
use eliteforge_agent_official_plugin_otel::{
    OtelRuntimeConfig, OtelRuntimePlugin, OtelRuntimeService,
};
use eliteforge_agent_official_plugin_otel_collector::{
    CollectorConfig, CollectorPlugin, CollectorService,
};
use eliteforge_agent_official_plugin_storage_local::TelemetryStoragePlugin;
use eliteforge_agent_plugin_core::PluginContainer;
use opentelemetry::{
    metrics::MeterProvider,
    trace::{Span, Tracer, TracerProvider},
};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
#[tokio::test]
async fn managed_runtime_flushes_before_receiver_and_store_close() {
    let temp = tempfile::tempdir().unwrap();
    let mut container = PluginContainer::build(vec![
        Arc::new(TelemetryStoragePlugin::new(
            temp.path().join("otel.db"),
            TelemetryRetention {
                trace_max_age: Duration::from_secs(86400),
                metric_max_age: Duration::from_secs(86400),
                max_completed_traces: Some(100),
            },
        )),
        Arc::new(CollectorPlugin {
            storage_plugin_id: "storage.telemetry".into(),
            config: CollectorConfig {
                bind: "127.0.0.1:0".parse().unwrap(),
                authorization: "Bearer runtime-test".into(),
                max_concurrent_requests: 2,
                cleanup_interval: Duration::from_secs(3600),
            },
        }),
        Arc::new(OtelRuntimePlugin {
            config: OtelRuntimeConfig {
                resource: BTreeMap::from([("service.name".into(), "example-host".into())]),
                process_metrics: true,
                use_proxy: false,
                ..Default::default()
            },
        }),
    ])
    .await
    .unwrap();
    let runtime = container.services().get::<OtelRuntimeService>().unwrap();
    let store = container
        .services()
        .get::<TelemetryStoreService>()
        .unwrap()
        .0
        .clone();
    let endpoint = container
        .services()
        .get::<CollectorService>()
        .unwrap()
        .endpoint
        .clone();
    let meter = runtime.metrics.meter("test");
    let count = meter.u64_counter("requests").build();
    count.add(7, &[]);
    let span = runtime.traces.tracer("test").start("generic.operation");
    let id = span.span_context().trace_id().to_string();
    drop(span);
    tokio::time::sleep(Duration::from_millis(1200)).await;
    runtime.force_flush().await.unwrap();
    assert_eq!(
        store
            .metric_points(Some("requests".into()).into(), 0, 10)
            .await
            .unwrap()
            .records[0]
            .data["asInt"],
        "7"
    );
    assert!(
        store
            .latest_metric(
                "process.memory.usage".into(),
                BTreeMap::from([("service.instance.id".into(), runtime.instance_id.clone())])
            )
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(store.spans(id).await.unwrap().len(), 1);
    // Leave a new point buffered: shutdown must deliver it before closing SQLite.
    count.add(5, &[]);
    let report = container.close_async(Duration::from_secs(10)).await;
    assert!(report.is_success(), "{report:?}");
    assert!(
        container
            .close_async(Duration::from_secs(10))
            .await
            .is_success()
    );
    assert!(store.metric_points(None.into(), 0, 1).await.is_err());
    let reopened = eliteforge_agent_official_plugin_storage_local::SqliteTelemetryStore::open(
        temp.path().join("otel.db"),
        TelemetryRetention {
            trace_max_age: Duration::from_secs(86400),
            metric_max_age: Duration::from_secs(86400),
            max_completed_traces: Some(100),
        },
    )
    .await
    .unwrap();
    let points = reopened
        .metric_points(Some("requests".into()).into(), 0, 10)
        .await
        .unwrap();
    assert!(points.records.iter().any(|p| p.data["asInt"] == "12"));
    assert!(
        tokio::net::TcpStream::connect(endpoint.strip_prefix("http://").unwrap())
            .await
            .is_err()
    );
}
