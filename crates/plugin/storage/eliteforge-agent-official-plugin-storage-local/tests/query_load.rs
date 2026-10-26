//! Opt-in load acceptance against the file-backed WAL runtime, without external services.
use eliteforge_agent_core::*;
use eliteforge_agent_official_plugin_storage_local::*;
use rusqlite::params;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

static SELECTS: AtomicUsize = AtomicUsize::new(0);
fn trace(event: rusqlite::trace::TraceEvent<'_>) {
    if let rusqlite::trace::TraceEvent::Stmt(statement, _) = event {
        let sql = statement.sql();
        if sql.trim_start().starts_with("SELECT") || sql.trim_start().starts_with("WITH") {
            SELECTS.fetch_add(1, Ordering::SeqCst);
        }
    }
}
async fn seed_history(database: &SqliteDatabase, start: usize, end: usize) {
    database.call(move |c| -> rusqlite::Result<()> {
        let tx = c.transaction()?;
        {
            let mut runs = tx.prepare("INSERT INTO ef_history_runs(id,session_id,owner_session_id,root_run_id,active,record) VALUES(?1,?2,?2,?3,0,?4)")?;
            let mut messages = tx.prepare("INSERT INTO ef_history_messages VALUES(?1,?2,?3,0,?4)")?;
            for index in start..end {
                let id = RunId::from_u128(index as u128 + 1);
                let root = RunId::from_u128((index / 3 * 3) as u128 + 1);
                let run = HistoryRun {id,session_id:"load-owner".into(),parent_run_id:(id != root).then_some(root),root_run_id:root,status:HistoryStatus::Completed,started_at_ms:index as u64,finished_at_ms:Some(index as u64),error:None};
                runs.execute(params![id.to_string(),run.session_id,root.to_string(),serde_json::to_string(&run).unwrap()])?;
                let message = HistoryMessage::user(id,&Content::from(format!("history {index}: 并发与数据库查询")));
                messages.execute(params![message.id,id.to_string(),run.session_id,serde_json::to_string(&message).unwrap()])?;
                if index % 10 == 0 {
                    let mut foreign = run;
                    foreign.id = RunId::from_u128(1_000_000 + index as u128);
                    foreign.session_id = "load-other".into();
                    runs.execute(params![foreign.id.to_string(),foreign.session_id,root.to_string(),serde_json::to_string(&foreign).unwrap()])?;
                }
            }
        }
        tx.commit()
    }).await.unwrap();
}

fn summary(
    rows: usize,
    workers: usize,
    mut times: Vec<f64>,
    elapsed: f64,
    selects: usize,
    write_batches: usize,
) {
    times.sort_by(f64::total_cmp);
    let percentile = |p: usize| times[((times.len() - 1) * p).div_ceil(100)];
    eprintln!(
        "{}",
        serde_json::json!({"suite":"framework-history-interaction-concurrent","historyRuns":rows,"workers":workers,"readRequests":times.len(),"writeBatches":write_batches,"selects":selects,"p50Ms":percentile(50),"p95Ms":percentile(95),"maxMs":times.last(),"readRequestsPerSecond":times.len() as f64/elapsed})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "local load test: 100002 history runs, 10000 interactions and 32 concurrent readers plus a writer"]
async fn large_data_concurrency_query_load() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("load.sqlite");
    let database = SqliteDatabase::open(&path).await.unwrap();
    let store = Arc::new(
        SqliteStateStore::with_database(database.clone(), SqliteOptions::default())
            .await
            .unwrap(),
    );
    database
        .call(|c| -> rusqlite::Result<()> {
            for id in ["load-owner", "load-other"] {
                let mut record = SessionRecord::new(id);
                record.id = id.into();
                c.execute("INSERT INTO ef_session_ids VALUES(?1,0)", [id])?;
                c.execute(
                    "INSERT INTO ef_sessions VALUES(?1,NULL,0,'Active',?2)",
                    params![id, serde_json::to_string(&record).unwrap()],
                )?;
            }
            c.trace_v2(
                rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
                Some(trace),
            );
            Ok(())
        })
        .await
        .unwrap();
    for _ in 0..4 {
        database
            .call_read(|c| {
                c.trace_v2(
                    rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
                    Some(trace),
                );
                Ok::<_, rusqlite::Error>(())
            })
            .await
            .unwrap();
    }
    let mut start = 0;
    for size in [300, 3_000, 30_000, 100_002] {
        seed_history(&database, start, size).await;
        start = size;
        for limit in [1, 20, 100] {
            SELECTS.store(0, Ordering::SeqCst);
            let now = Instant::now();
            let page = store
                .read_session(
                    "load-owner",
                    HistoryQuery {
                        limit,
                        cursor: None,
                    },
                )
                .await
                .unwrap();
            let elapsed = now.elapsed().as_secs_f64() * 1_000.0;
            let count = SELECTS.load(Ordering::SeqCst);
            assert_eq!(count, 10, "history queries grew with database or page size");
            assert_eq!(page.runs.len(), limit * 3);
            assert_eq!(page.records.messages.len(), limit * 3);
            assert!(page.runs.iter().all(|run| run.session_id == "load-owner"));
            eprintln!(
                "{}",
                serde_json::json!({"suite":"framework-history-scaling","historyRuns":size,"limit":limit,"selects":count,"elapsedMs":elapsed})
            );
        }
    }
    let prototype = InteractionRecord {
        session_id: "load-owner".into(),
        run_id: RunId::from_u128(1),
        node_invocation_id: None,
        request: InteractionRequest::input("Choose", serde_json::json!({"type":"string"})).unwrap(),
        tool_call: None,
        response: None,
        response_provenance: None,
        created_at_unix_ms: 1,
        answered_at_unix_ms: None,
    };
    let template = prototype.clone();
    database
        .call(move |c| -> rusqlite::Result<()> {
            let tx = c.transaction()?;
            {
                let mut statement =
                    tx.prepare("INSERT INTO ef_interactions VALUES(?1,'load-owner',?2,0,?3)")?;
                for index in 0..10_000 {
                    let mut record = template.clone();
                    record.request.id = RunId::from_u128(2_000_000 + index);
                    statement.execute(params![
                        record.request.id.to_string(),
                        record.run_id.to_string(),
                        serde_json::to_string(&record).unwrap()
                    ])?;
                }
            }
            tx.commit()
        })
        .await
        .unwrap();
    for size in [1, 1_000, 10_000] {
        let ids = (0..size)
            .rev()
            .map(|index| RunId::from_u128(2_000_000 + index))
            .collect::<Vec<_>>();
        SELECTS.store(0, Ordering::SeqCst);
        let now = Instant::now();
        let found = store.get_many("load-owner", &ids).await.unwrap();
        let elapsed = now.elapsed().as_secs_f64() * 1_000.0;
        assert_eq!(SELECTS.load(Ordering::SeqCst), 1);
        assert_eq!(
            found
                .iter()
                .map(|record| record.request.id)
                .collect::<Vec<_>>(),
            ids
        );
        eprintln!(
            "{}",
            serde_json::json!({"suite":"framework-interaction-scaling","rows":size,"selects":1,"elapsedMs":elapsed})
        );
    }
    let workers = 32;
    let rounds = 1_000;
    let barrier = Arc::new(tokio::sync::Barrier::new(workers + 2));
    let mut readers = Vec::new();
    for _ in 0..workers {
        let store = store.clone();
        let barrier = barrier.clone();
        readers.push(tokio::spawn(async move {
            let ids = (0..100)
                .map(|index| RunId::from_u128(2_000_000 + index))
                .collect::<Vec<_>>();
            barrier.wait().await;
            let mut times = Vec::new();
            for round in 0..rounds {
                let now = Instant::now();
                tokio::time::timeout(Duration::from_secs(10), async {
                    if round % 2 == 0 {
                        let page = store
                            .read_session(
                                "load-owner",
                                HistoryQuery {
                                    limit: 20,
                                    cursor: None,
                                },
                            )
                            .await
                            .unwrap();
                        assert_eq!(page.runs.len(), 60);
                        assert_eq!(page.records.messages.len(), 60);
                        assert!(page.runs.iter().all(|run| run.session_id == "load-owner"));
                    } else {
                        let found = store.get_many("load-owner", &ids).await.unwrap();
                        assert_eq!(
                            found
                                .iter()
                                .map(|record| record.request.id)
                                .collect::<Vec<_>>(),
                            ids
                        );
                    }
                })
                .await
                .expect("read starved under write contention");
                times.push(now.elapsed().as_secs_f64() * 1_000.0);
            }
            times
        }));
    }
    let writing = store.clone();
    let writer_barrier = barrier.clone();
    let writer = tokio::spawn(async move {
        writer_barrier.wait().await;
        let mut published = Vec::new();
        for batch in 0..rounds {
            let records = (0..32)
                .map(|index| {
                    let mut record = prototype.clone();
                    record.request.id = RunId::from_u128(3_000_000 + (batch * 32 + index) as u128);
                    published.push(record.request.id);
                    record
                })
                .collect::<Vec<_>>();
            tokio::time::timeout(Duration::from_secs(10), writing.publish_many(&records))
                .await
                .expect("writer starved under read contention")
                .unwrap();
            tokio::task::yield_now().await;
        }
        published
    });
    SELECTS.store(0, Ordering::SeqCst);
    let now = Instant::now();
    barrier.wait().await;
    let mut times = Vec::new();
    for reader in readers {
        times.extend(reader.await.unwrap());
    }
    let published = writer.await.unwrap();
    let elapsed = now.elapsed().as_secs_f64();
    let count = SELECTS.load(Ordering::SeqCst);
    assert_eq!(
        count,
        workers * (rounds / 2) * (10 + 1) + rounds,
        "read/write contention caused query amplification"
    );
    assert_eq!(
        store
            .get_many("load-owner", &published)
            .await
            .unwrap()
            .len(),
        rounds * 32
    );
    let mut sorted = times.clone();
    sorted.sort_by(f64::total_cmp);
    let p95 = sorted[((sorted.len() - 1) * 95).div_ceil(100)];
    assert!(
        p95 < 500.0,
        "local mixed read P95 exceeded 500 ms: {p95} ms"
    );
    summary(100_002, workers, times, elapsed, count, rounds);
    store.close().await.unwrap();
}
