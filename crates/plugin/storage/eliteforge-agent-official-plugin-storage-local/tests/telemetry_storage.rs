use eliteforge_agent_core::*;
use eliteforge_agent_official_plugin_storage_local::{SqliteDatabase, SqliteTelemetryStore};
use std::{
    collections::BTreeMap,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
fn policy() -> TelemetryRetention {
    TelemetryRetention {
        trace_max_age: Duration::from_secs(15 * 86400),
        metric_max_age: Duration::from_secs(5 * 86400),
        max_completed_traces: Some(50_000),
    }
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[tokio::test]
async fn counts_metric_points_by_resource_or_point_attribute_without_loading_points() {
    let temp = tempfile::tempdir().unwrap();
    let database = SqliteDatabase::open(temp.path().join("metrics.db")).await.unwrap();
    let store = SqliteTelemetryStore::with_database(database.clone(), policy())
        .await
        .unwrap();
    store
        .write_metrics(vec![
            TelemetryMetricPoint {
                point_id: "resource-alpha".into(), timestamp: now(),
                resource: BTreeMap::from([("tron.workspace.code".into(), "alpha".into())]),
                ..Default::default()
            },
            TelemetryMetricPoint {
                point_id: "attribute-alpha".into(), timestamp: now(),
                data: serde_json::json!({
                    "attributes": [{
                        "key": "tron.workspace.code",
                        "value": {"stringValue": "alpha"}
                    }]
                }),
                ..Default::default()
            },
            TelemetryMetricPoint {
                point_id: "other-workspace".into(), timestamp: now(),
                resource: BTreeMap::from([("tron.workspace.code".into(), "beta".into())]),
                ..Default::default()
            },
        ])
        .await
        .unwrap();
    assert_eq!(
        store
            .count_metric_points_by_attribute("tron.workspace.code".into(), "alpha".into())
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        store
            .count_metric_points_by_attribute("tron.workspace.code".into(), "beta".into())
            .await
            .unwrap(),
        1
    );
    store.cleanup().await.unwrap();
}

#[tokio::test]
async fn retention_removes_entire_traces_and_only_ages_out_metrics() {
    let temp = tempfile::tempdir().unwrap();
    let db = SqliteDatabase::open(temp.path().join("data.db"))
        .await
        .unwrap();
    let store = SqliteTelemetryStore::with_database(db.clone(), policy())
        .await
        .unwrap();
    db.call(|c| -> rusqlite::Result<()> {
        let now = now();
        let tx = c.transaction()?;
        for i in 0..50_002 {
            tx.execute(
                "INSERT INTO ef_telemetry_traces VALUES(?1,?2,?2,0,'{}',0)",
                rusqlite::params![format!("{i:032x}"), now - i],
            )?;
        }
        tx.execute(
            "INSERT INTO ef_telemetry_traces VALUES('expired',?1,?1,0,'{}',0)",
            [now - 16 * 86400000],
        )?;
        tx.execute(
            "INSERT INTO ef_telemetry_traces VALUES('active',?1,NULL,0,'{}',0)",
            [now - 20 * 86400000],
        )?;
        for id in ["expired", "0000000000000000000000000000c351"] {
            tx.execute(
                "INSERT INTO ef_telemetry_spans VALUES(?1,'span',?2,'{}')",
                rusqlite::params![id, now],
            )?;
        }
        for (id, days) in [("old", 6), ("kept", 4)] {
            tx.execute(
                "INSERT INTO ef_telemetry_metric_points VALUES(?1,'m',?2,'{}')",
                rusqlite::params![id, now - days * 86400000],
            )?;
        }
        for i in 0..50_001 {
            tx.execute(
                "INSERT INTO ef_telemetry_metric_points VALUES(?1,'m',?2,'{}')",
                rusqlite::params![format!("fresh-{i}"), now],
            )?;
        }
        tx.commit()
    })
    .await
    .unwrap();
    store.cleanup().await.unwrap();
    db.call(|c| -> rusqlite::Result<()> {
        for (sql, expected) in [
            (
                "SELECT COUNT(*) FROM ef_telemetry_traces WHERE ended_at IS NOT NULL",
                50_000,
            ),
            (
                "SELECT COUNT(*) FROM ef_telemetry_traces WHERE trace_id='active'",
                1,
            ),
            ("SELECT COUNT(*) FROM ef_telemetry_spans", 0),
            ("SELECT COUNT(*) FROM ef_telemetry_metric_points", 50_002),
            (
                "SELECT COUNT(*) FROM ef_telemetry_metric_points WHERE point_id='old'",
                0,
            ),
        ] {
            assert_eq!(c.query_row(sql, [], |r| r.get::<_, i64>(0))?, expected);
        }
        Ok(())
    })
    .await
    .unwrap();
    db.close().await.unwrap();
}
#[tokio::test]
async fn generic_filters_root_metadata_retries_and_restart_recovery() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("data.db");
    let store = SqliteTelemetryStore::open(&path, policy()).await.unwrap();
    let attrs = BTreeMap::from([
        ("tenant".into(), "a".into()),
        ("result".into(), "root".into()),
    ]);
    store
        .register_trace(TelemetryTrace {
            trace_id: "trace".into(),
            started_at: now(),
            attributes: attrs.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
    let root = TelemetrySpan {
        trace_id: "trace".into(),
        span_id: "root".into(),
        name: "operation".into(),
        started_at: now(),
        ended_at: now() + 1,
        attributes: attrs.clone(),
        ..Default::default()
    };
    let child = TelemetrySpan {
        trace_id: "trace".into(),
        span_id: "child".into(),
        parent_span_id: Some("root".into()),
        name: "model".into(),
        started_at: now(),
        ended_at: now() + 1,
        attributes: BTreeMap::from([
            ("result".into(), "child".into()),
            ("tokens".into(), "23".into()),
        ]),
        ..Default::default()
    };
    store
        .write_spans(vec![root.clone(), child.clone()])
        .await
        .unwrap();
    store.write_spans(vec![child]).await.unwrap();
    let filter = TelemetryTraceFilter {
        attributes: BTreeMap::from([("tenant".into(), "a".into())]),
        ..Default::default()
    };
    let page = store.query_traces(filter.clone(), 0, 10).await.unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.records[0].attributes["result"], "root");
    assert_eq!(store.spans("trace".into()).await.unwrap().len(), 2);
    assert_eq!(
        store
            .sum_span_attributes(filter, "model".into(), vec!["tokens".into()])
            .await
            .unwrap()["tokens"],
        23
    );
    assert_eq!(
        store
            .query_traces(
                TelemetryTraceFilter {
                    attributes: BTreeMap::from([("tenant".into(), "b".into())]),
                    ..Default::default()
                },
                0,
                10
            )
            .await
            .unwrap()
            .total,
        0
    );
    store
        .register_trace(TelemetryTrace {
            trace_id: "unfinished".into(),
            started_at: now(),
            ..Default::default()
        })
        .await
        .unwrap();
    drop(store);
    let reopened = SqliteTelemetryStore::open(&path, policy()).await.unwrap();
    let page = reopened
        .query_traces(
            TelemetryTraceFilter {
                trace_id: Some("unfinished".into()),
                ..Default::default()
            },
            0,
            1,
        )
        .await
        .unwrap();
    assert!(page.records[0].abandoned);
    assert!(page.records[0].ended_at.is_some());
}

#[tokio::test]
async fn late_business_registration_preserves_browser_root_and_completion_time() {
    let temp = tempfile::tempdir().unwrap();
    let store = SqliteTelemetryStore::open(temp.path().join("late.db"), policy()).await.unwrap();
    let start = now();
    let browser = TelemetrySpan {
        trace_id: "trace".into(), span_id: "browser".into(), started_at: start, ended_at: start + 10,
        attributes: BTreeMap::from([("service.name".into(), "frontend".into())]), ..Default::default()
    };
    store.write_spans(vec![browser.clone()]).await.unwrap();
    store.register_trace(TelemetryTrace {
        trace_id: "trace".into(), started_at: start + 2,
        attributes: BTreeMap::from([("workspace".into(), "a".into())]), ..Default::default()
    }).await.unwrap();
    store.register_trace(TelemetryTrace {
        trace_id: "trace".into(), started_at: start + 2, ended_at: Some(start + 100), status_code: 2,
        attributes: BTreeMap::from([("outcome".into(), "error".into())]), ..Default::default()
    }).await.unwrap();
    // A delayed/retried browser export cannot truncate a longer background child execution.
    store.write_spans(vec![browser]).await.unwrap();
    let page = store.query_traces(TelemetryTraceFilter::default(), 0, 1).await.unwrap();
    let trace = &page.records[0];
    assert_eq!(trace.started_at, start);
    assert_eq!(trace.ended_at, Some(start + 100));
    assert_eq!(trace.status_code, 2);
    assert_eq!(trace.attributes["workspace"], "a");
    assert_eq!(trace.attributes["service.name"], "frontend");
}

#[tokio::test]
async fn clear_removes_metrics_traces_and_rollups_and_allows_new_collection() {
    let temp = tempfile::tempdir().unwrap();
    let db = SqliteDatabase::open(temp.path().join("reset.db")).await.unwrap();
    let store = SqliteTelemetryStore::with_database(db.clone(), policy()).await.unwrap();
    store.register_trace(TelemetryTrace { trace_id: "old".into(), started_at: now(), ended_at: Some(now()), ..Default::default() }).await.unwrap();
    store.write_spans(vec![TelemetrySpan { trace_id: "old".into(), span_id: "span".into(), name: "request".into(), started_at: now(), ended_at: now(), status_code: 2, attributes: BTreeMap::from([("error.count".into(), "1".into())]), ..Default::default() }]).await.unwrap();
    store.write_metrics(vec![TelemetryMetricPoint { point_id: "old-point".into(), name: "memory".into(), timestamp: now(), resource: BTreeMap::from([("service.name".into(), "test".into())]), ..Default::default() }]).await.unwrap();
    store.clear().await.unwrap();
    db.call(|c| -> rusqlite::Result<()> {
        for table in ["ef_telemetry_metric_points", "ef_telemetry_metric_attributes", "ef_telemetry_spans", "ef_telemetry_span_attributes", "ef_telemetry_trace_attributes", "ef_telemetry_traces", "ef_telemetry_span_rollups", "ef_telemetry_trace_rollups"] {
            let count: i64 = c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
            assert_eq!(count, 0, "{table}");
        }
        Ok(())
    }).await.unwrap();
    store.clear().await.unwrap();
    store.write_metrics(vec![TelemetryMetricPoint { point_id: "new".into(), timestamp: now(), ..Default::default() }]).await.unwrap();
    assert_eq!(store.metric_points(None.into(), 0, 10).await.unwrap().total, 1);
}
