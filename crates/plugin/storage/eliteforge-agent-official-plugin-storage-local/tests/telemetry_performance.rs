//! Native telemetry contracts, cardinality budgets and concurrent read/write acceptance.
use eliteforge_agent_core::*;
use eliteforge_agent_official_plugin_storage_local::*;
use eliteforge_agent_test_support::storage_contracts::*;
use rusqlite::params;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
static LOCK: Mutex<()> = Mutex::new(());
static SELECTS: AtomicUsize = AtomicUsize::new(0);
fn trace(event: rusqlite::trace::TraceEvent<'_>) {
    if let rusqlite::trace::TraceEvent::Stmt(s, _) = event {
        let sql = s.sql();
        if sql.trim_start().starts_with("SELECT") || sql.trim_start().starts_with("WITH") {
            SELECTS.fetch_add(1, Ordering::SeqCst);
        }
    }
}
fn policy() -> TelemetryRetention {
    TelemetryRetention {
        trace_max_age: Duration::from_secs(30 * 86400),
        metric_max_age: Duration::from_secs(30 * 86400),
        max_completed_traces: None,
    }
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}
fn tf() -> TelemetryTraceFilter {
    TelemetryTraceFilter {
        attributes: BTreeMap::from([("tenant".into(), "alpha".into())]),
        ..Default::default()
    }
}
fn mf() -> TelemetryMetricFilter {
    TelemetryMetricFilter {
        name: Some("load.metric".into()),
        attributes: BTreeMap::from([("tenant".into(), "alpha".into())]),
    }
}
async fn fixture() -> (tempfile::TempDir, SqliteDatabase, Arc<dyn TelemetryStore>) {
    let directory = tempfile::tempdir().unwrap();
    let db = SqliteDatabase::open(directory.path().join("telemetry.sqlite"))
        .await
        .unwrap();
    let store = Arc::new(
        SqliteTelemetryStore::with_database(db.clone(), policy())
            .await
            .unwrap(),
    );
    for _ in 0..4 {
        db.call_read(|c| {
            c.trace_v2(
                rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
                Some(trace),
            );
            Ok::<_, rusqlite::Error>(())
        })
        .await
        .unwrap();
    }
    (directory, db, store)
}
async fn seed(db: &SqliteDatabase, traces: usize, points: usize) {
    db.call(move|c|->rusqlite::Result<()> {
        let tx=c.transaction()?;let at=now();
        {
            let mut trace=tx.prepare("INSERT INTO ef_telemetry_traces VALUES(?1,?2,?3,?4,?5,0)")?;
            let mut span=tx.prepare("INSERT INTO ef_telemetry_spans VALUES(?1,?2,?3,?4)")?;
            for i in 0..traces {
                let id=format!("trace-{i:08}");let attrs=BTreeMap::from([("tenant".to_owned(),if i%2==0 {"alpha"}else{"beta"}.to_owned())]);
                trace.execute(params![id,at-i as i64,at-i as i64+(i%1001) as i64,if i%7==0 {2}else{0},serde_json::to_string(&attrs).unwrap()])?;
                for child in 0..6 {
                    let s=TelemetrySpan {trace_id:id.clone(),span_id:format!("span-{child}"),name:"agent.model".into(),started_at:at-i as i64,ended_at:at+1,attributes:BTreeMap::from([("tokens".into(),"3".into())]),..Default::default()};
                    span.execute(params![id,s.span_id,s.ended_at,serde_json::to_string(&s).unwrap()])?;
                }
            }
            let mut point=tx.prepare("INSERT INTO ef_telemetry_metric_points VALUES(?1,?2,?3,?4)")?;
            for i in 0..points {
                let tenant=if i%2==0 {"alpha"}else{"beta"};
                let p=TelemetryMetricPoint {point_id:format!("point-{i:08}"),name:"load.metric".into(),timestamp:at-i as i64,
                    resource:if i%4<2 {BTreeMap::from([("tenant".into(),tenant.into())])}else{BTreeMap::new()},
                    data:serde_json::json!({"attributes":[{"key":"tenant","value":{"stringValue":tenant}}],"asInt":"7"}),..Default::default()};
                point.execute(params![p.point_id,p.name,p.timestamp,serde_json::to_string(&p).unwrap()])?;
            }
        }
        tx.commit()
    }).await.unwrap();
}
async fn read(store: &dyn TelemetryStore, kind: usize, rows: usize, points: usize) {
    match kind {
        0 => {
            let page = store.query_traces(tf(), 0, 20).await.unwrap();
            assert_eq!(page.total, rows.div_ceil(2));
            assert_eq!(page.records.len(), 20.min(page.total));
            assert!(
                page.records
                    .iter()
                    .all(|t| t.attributes["tenant"] == "alpha")
            );
        }
        1 => {
            let page = store.metric_points(mf(), 0, 20).await.unwrap();
            assert_eq!(page.total, points.div_ceil(2));
            assert_eq!(page.records.len(), 20.min(page.total));
            assert!(
                page.records
                    .iter()
                    .all(|p| p.data["attributes"][0]["value"]["stringValue"] == "alpha")
            );
        }
        2 => {
            let page = store
                .query_spans(
                    vec![
                        "trace-00000000".into(),
                        "trace-00000000".into(),
                        "missing".into(),
                    ],
                    0,
                    3,
                )
                .await
                .unwrap();
            assert_eq!(page.total, 6);
            assert_eq!(page.records.len(), 3);
            assert!(page.records.iter().all(|s| s.trace_id == "trace-00000000"));
        }
        3 => {
            let sums = store
                .sum_span_attributes(
                    tf(),
                    "agent.model".into(),
                    vec!["tokens".into(), "absent".into()],
                )
                .await
                .unwrap();
            assert_eq!(sums["tokens"], rows.div_ceil(2) as u64 * 18);
            assert_eq!(sums["absent"], 0);
        }
        4 => {
            let at = now();
            let stats = store
                .aggregate_traces(
                    tf(),
                    BTreeMap::from([("error".into(), TelemetryTracePredicate::StatusCode(2))]),
                    Some(TelemetryBucketQuery {
                        start: at / 60_000 * 60_000 - 14 * 60_000,
                        end: at,
                        width_ms: 60_000,
                    }),
                )
                .await
                .unwrap();
            assert_eq!(stats.total, rows.div_ceil(2));
            assert_eq!(
                stats.classified["error"],
                (0..rows).filter(|i| i % 14 == 0).count()
            );
            assert_eq!(
                stats.buckets.iter().map(|b| b.total).sum::<usize>(),
                stats.total
            );
        }
        5 => assert_eq!(
            store
                .count_metric_points_by_attribute("tenant".into(), "alpha".into())
                .await
                .unwrap(),
            points.div_ceil(2)
        ),
        _ => assert_eq!(
            store
                .latest_metric(
                    "load.metric".into(),
                    BTreeMap::from([("tenant".into(), "alpha".into())])
                )
                .await
                .unwrap()
                .unwrap()
                .point_id,
            "point-00000000"
        ),
    }
}
const BUDGETS: [usize; 7] = [2, 2, 2, 1, 2, 1, 1];
const NAMES: [&str; 7] = [
    "trace-page",
    "metric-page",
    "span-batch-page",
    "span-aggregate",
    "trace-statistics",
    "metric-count",
    "latest-metric",
];
#[tokio::test]
async fn telemetry_pages_and_aggregates_obey_shared_contract() {
    let _guard = LOCK.lock().unwrap();
    for rows in [1, 100, 1000] {
        let (_dir, db, store) = fixture().await;
        seed(&db, rows, rows * 2).await;
        for kind in 0..7 {
            let before = SELECTS.load(Ordering::SeqCst);
            read(store.as_ref(), kind, rows, rows * 2).await;
            assert_query_budget(
                NAMES[kind],
                before,
                SELECTS.load(Ordering::SeqCst),
                BUDGETS[kind],
            );
        }
        let first = store.metric_points(mf(), 0, 3).await.unwrap();
        let second = store.metric_points(mf(), 3, 3).await.unwrap();
        assert!(
            first
                .records
                .iter()
                .all(|a| second.records.iter().all(|b| a.point_id != b.point_id))
        );
        assert!(store.query_traces(tf(), 0, 0).await.is_err());
        assert!(store.metric_points(mf(), 0, 50_001).await.is_err());
        let empty = store
            .query_spans(vec!["missing".into()], usize::MAX.min(i64::MAX as usize), 1)
            .await
            .unwrap();
        assert_eq!(empty.total, 0);
        assert!(empty.records.is_empty());
        let page = store
            .metric_points(mf(), usize::MAX.min(i64::MAX as usize), 1)
            .await
            .unwrap();
        assert_eq!(page.total, rows);
        assert!(page.records.is_empty());
        eprintln!(
            "{}",
            serde_json::json!({"suite":"telemetry-scaling","traces":rows,"points":rows*2,"selects":BUDGETS})
        );
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "10000 traces, 60000 spans, 100000 metric points, 16 readers and live ingestion"]
async fn telemetry_large_data_concurrency_load() {
    let _guard = LOCK.lock().unwrap();
    let (_dir, db, store) = fixture().await;
    let rows = 10_000;
    let points = 100_000;
    seed(&db, rows, points).await;
    for kind in 0..7 {
        let started = Instant::now();
        read(store.as_ref(), kind, rows, points).await;
        eprintln!(
            "{}",
            serde_json::json!({"suite":"telemetry-single","operation":NAMES[kind],"elapsedMs":started.elapsed().as_secs_f64()*1000.})
        );
    }
    let writing = store.clone();
    let start = Arc::new(tokio::sync::Barrier::new(2));
    let writer_start = start.clone();
    let writer = tokio::spawn(async move {
        writer_start.wait().await;
        let started = Instant::now();
        let mut samples = Vec::new();
        for batch in 0..100 {
            let at = now();
            let begin = Instant::now();
            let metrics = (0..16)
                .map(|i| TelemetryMetricPoint {
                    point_id: format!("live-{batch}-{i}"),
                    name: "live".into(),
                    timestamp: at,
                    resource: BTreeMap::from([("tenant".into(), "writer".into())]),
                    ..Default::default()
                })
                .collect();
            tokio::time::timeout(Duration::from_secs(10), writing.write_metrics(metrics))
                .await
                .unwrap()
                .unwrap();
            let spans = (0..16)
                .map(|i| TelemetrySpan {
                    trace_id: format!("writer-{batch}"),
                    span_id: format!("live-{i}"),
                    started_at: at,
                    ended_at: at + 1,
                    attributes: BTreeMap::from([("tenant".into(), "writer".into())]),
                    ..Default::default()
                })
                .collect();
            tokio::time::timeout(Duration::from_secs(10), writing.write_spans(spans))
                .await
                .unwrap()
                .unwrap();
            samples.push(begin.elapsed().as_secs_f64() * 1000.);
            tokio::task::yield_now().await;
        }
        ReadLoad {
            milliseconds: samples,
            elapsed_seconds: started.elapsed().as_secs_f64(),
        }
    });
    let times = Arc::new(Mutex::new((0..7).map(|_| Vec::new()).collect::<Vec<_>>()));
    let recorded = times.clone();
    let reading = store.clone();
    SELECTS.store(0, Ordering::SeqCst);
    start.wait().await;
    let load = concurrent_reads(16, 70, move |worker, round| {
        let store = reading.clone();
        let times = recorded.clone();
        async move {
            let kind = (worker + round) % 7;
            let at = Instant::now();
            read(store.as_ref(), kind, rows, points).await;
            times.lock().unwrap()[kind].push(at.elapsed().as_secs_f64() * 1000.);
        }
    })
    .await;
    let count = SELECTS.load(Ordering::SeqCst);
    let writes = writer.await.unwrap();
    let expected = (0..16)
        .flat_map(|worker| (0..70).map(move |round| BUDGETS[(worker + round) % 7]))
        .sum::<usize>();
    assert_query_budget("concurrent telemetry reads", 0, count, expected);
    for (kind, milliseconds) in times.lock().unwrap().iter().enumerate() {
        let sample = ReadLoad {
            milliseconds: milliseconds.clone(),
            elapsed_seconds: load.elapsed_seconds,
        };
        eprintln!("{}", sample.report(NAMES[kind]));
    }
    for (kind, milliseconds) in times.lock().unwrap().iter().enumerate() {
        ReadLoad {
            milliseconds: milliseconds.clone(),
            elapsed_seconds: load.elapsed_seconds,
        }
        .assert_p95(NAMES[kind], 1000.);
    }
    load.assert_p95("mixed telemetry", 1000.);
    writes.assert_p95("telemetry ingestion batches", 1000.);
    eprintln!("{}", load.report("telemetry-mixed-concurrent"));
    eprintln!("{}", writes.report("telemetry-write-batches"));
    eprintln!(
        "{}",
        serde_json::json!({"suite":"telemetry-query-budget","readSelects":count,"expected":expected,"readers":16,"traceRows":rows,"spanRows":rows*6,"metricRows":points,"ingestedSpans":1600,"ingestedMetricPoints":1600})
    );
}
