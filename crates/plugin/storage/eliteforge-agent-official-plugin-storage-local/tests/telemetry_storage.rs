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
