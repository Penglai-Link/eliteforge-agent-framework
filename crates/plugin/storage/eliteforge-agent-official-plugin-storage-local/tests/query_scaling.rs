use eliteforge_agent_core::*;
use eliteforge_agent_official_plugin_storage_local::*;
use rusqlite::params;
use std::sync::atomic::{AtomicUsize, Ordering};

static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static SELECTS: AtomicUsize = AtomicUsize::new(0);
fn count_select(event: rusqlite::trace::TraceEvent<'_>) {
    if let rusqlite::trace::TraceEvent::Stmt(statement, _) = event
        && (statement.sql().trim_start().starts_with("SELECT")
            || statement.sql().trim_start().starts_with("WITH"))
    {
        SELECTS.fetch_add(1, Ordering::SeqCst);
    }
}

async fn fixture(database: &SqliteDatabase) -> SqliteStateStore {
    let store = SqliteStateStore::with_database(database.clone(), SqliteOptions::default())
        .await
        .unwrap();
    database.call(|c| -> rusqlite::Result<()> {
        for session in ["owner", "other"] {
            let mut record = SessionRecord::new(session);
            record.id = session.into();
            c.execute("INSERT INTO ef_session_ids VALUES(?1,0)",[session])?;
            c.execute("INSERT INTO ef_sessions VALUES(?1,NULL,0,'Active',?2)",params![session,serde_json::to_string(&record).unwrap()])?;
        }
        for turn in 0..32 {
            let root = RunId::new_v4();
            for child in 0..3 {
                let id = if child == 0 { root } else { RunId::new_v4() };
                let run = HistoryRun { id,session_id:"owner".into(),parent_run_id:(child>0).then_some(root),root_run_id:root,
                    status:HistoryStatus::Completed,started_at_ms:turn,finished_at_ms:Some(turn),error:None };
                c.execute("INSERT INTO ef_history_runs(id,session_id,owner_session_id,root_run_id,active,record) VALUES(?1,'owner','owner',?2,0,?3)",params![id.to_string(),root.to_string(),serde_json::to_string(&run).unwrap()])?;
                let message = HistoryMessage::user(id,&Content::from(format!("turn {turn}, child {child}")));
                c.execute("INSERT INTO ef_history_messages VALUES(?1,?2,'owner',0,?3)",params![message.id,id.to_string(),serde_json::to_string(&message).unwrap()])?;
                // The same root in another session must never enter this page.
                if child == 0 {
                    let mut other = run;
                    other.id = RunId::new_v4(); other.session_id = "other".into();
                    c.execute("INSERT INTO ef_history_runs(id,session_id,owner_session_id,root_run_id,active,record) VALUES(?1,'other','other',?2,0,?3)",params![other.id.to_string(),root.to_string(),serde_json::to_string(&other).unwrap()])?;
                }
            }
        }
        Ok(())
    }).await.unwrap();
    store
}

#[tokio::test]
async fn page_and_batch_queries_do_not_scale_with_rows() {
    let _guard = TEST_LOCK.lock().unwrap();
    let database = SqliteDatabase::open(":memory:").await.unwrap();
    let store = fixture(&database).await;
    database
        .call(|c| {
            c.trace_v2(
                rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
                Some(count_select),
            );
            Ok::<_, rusqlite::Error>(())
        })
        .await
        .unwrap();
    let mut counts = Vec::new();
    for limit in [1, 20] {
        SELECTS.store(0, Ordering::SeqCst);
        let page = store
            .read_session(
                "owner",
                HistoryQuery {
                    limit,
                    cursor: None,
                },
            )
            .await
            .unwrap();
        let count = SELECTS.load(Ordering::SeqCst);
        eliteforge_agent_test_support::storage_contracts::assert_query_budget(
            "history page",
            0,
            count,
            10,
        );
        counts.push(count);
        assert_eq!(page.runs.len(), limit * 3);
        assert_eq!(page.records.messages.len(), limit * 3);
        assert!(page.runs.iter().all(|run| run.session_id == "owner"));
        assert!(
            page.records
                .messages
                .windows(2)
                .all(|pair| pair[0].content != pair[1].content)
        );
        assert!(page.next_cursor.is_some());
        if limit == 20 {
            let older = store
                .read_session(
                    "owner",
                    HistoryQuery {
                        limit: 20,
                        cursor: page.next_cursor,
                    },
                )
                .await
                .unwrap();
            assert_eq!(older.runs.len(), 12 * 3);
            assert!(older.next_cursor.is_none());
            assert!(older.runs.iter().all(|run| run.started_at_ms < 12));
        }
    }
    assert_eq!(
        counts[0], counts[1],
        "history SELECTs grew with page size: {counts:?}"
    );
    let page = store
        .read_session("owner", HistoryQuery::default())
        .await
        .unwrap();
    let ids = vec![page.runs[2].id, page.runs[0].id, page.runs[2].id];
    SELECTS.store(0, Ordering::SeqCst);
    let runs = store.runs(&ids).await.unwrap();
    assert_eq!(runs.iter().map(|run| run.id).collect::<Vec<_>>(), ids);
    assert_eq!(SELECTS.load(Ordering::SeqCst), 1);
    assert!(matches!(
        store.runs(&[RunId::new_v4()]).await,
        Err(StateError::NotFound)
    ));
    let missing = RunId::new_v4();
    assert_eq!(
        store.find_many("owner", &[missing, missing]).await.unwrap(),
        vec![None, None]
    );
    assert!(matches!(
        store.get_many("owner", &[missing]).await,
        Err(StateError::NotFound)
    ));
    eprintln!(
        "history SELECTs: limit=1: {}, limit=20: {}; run batch: 1",
        counts[0], counts[1]
    );
    let records = (0..1200)
        .map(|_| InteractionRecord {
            session_id: "owner".into(),
            run_id: ids[0],
            request: InteractionRequest::input("Choose", serde_json::json!({"type":"string"}))
                .unwrap(),
            tool_call: None,
            response: None,
            response_provenance: None,
            created_at_unix_ms: 1,
            answered_at_unix_ms: None,
        })
        .collect::<Vec<_>>();
    SELECTS.store(0, Ordering::SeqCst);
    store.publish_many(&records).await.unwrap();
    assert_eq!(
        SELECTS.load(Ordering::SeqCst),
        1,
        "batch publication must not query each record"
    );
    let requested = records
        .iter()
        .map(|record| record.request.id)
        .collect::<Vec<_>>();
    SELECTS.store(0, Ordering::SeqCst);
    let found = store.get_many("owner", &requested).await.unwrap();
    assert_eq!(found, records);
    assert_eq!(
        SELECTS.load(Ordering::SeqCst),
        1,
        "1200 records must use one query"
    );
    assert!(matches!(
        store.get_many("other", &requested).await,
        Err(StateError::NotFound)
    ));
    let mut new = records[0].clone();
    new.request.id = RunId::new_v4();
    let mut conflicting = records[0].clone();
    conflicting.request.prompt = "Changed".into();
    assert!(matches!(
        store.publish_many(&[new.clone(), conflicting]).await,
        Err(StateError::Conflict)
    ));
    assert!(
        matches!(
            store.get("owner", new.request.id).await,
            Err(StateError::NotFound)
        ),
        "conflicting publication must roll back the whole batch"
    );
    let repeated = store
        .get_many("owner", &[requested[2], requested[0], requested[2]])
        .await
        .unwrap();
    assert_eq!(
        repeated
            .iter()
            .map(|record| record.request.id)
            .collect::<Vec<_>>(),
        vec![requested[2], requested[0], requested[2]]
    );
    let mut deletion_counts = Vec::new();
    for children in [1, 40] {
        let owner = format!("deletion-{children}");
        let parent = owner.clone();
        database
            .call(move |c| -> rusqlite::Result<()> {
                for index in 0..=children {
                    let id = if index == 0 {
                        parent.clone()
                    } else {
                        format!("{parent}/{index}")
                    };
                    let mut record = SessionRecord::new(&id);
                    record.id = id.clone();
                    c.execute("INSERT INTO ef_session_ids VALUES(?1,0)", [&id])?;
                    c.execute(
                        "INSERT INTO ef_sessions VALUES(?1,?2,0,'Active',?3)",
                        params![
                            id,
                            (index > 0).then_some(&parent),
                            serde_json::to_string(&record).unwrap()
                        ],
                    )?;
                }
                Ok(())
            })
            .await
            .unwrap();
        SELECTS.store(0, Ordering::SeqCst);
        assert!(store.delete_session(&owner).await.unwrap());
        deletion_counts.push(SELECTS.load(Ordering::SeqCst));
    }
    assert_eq!(
        deletion_counts[0], deletion_counts[1],
        "owned-session deletion SELECTs grew: {deletion_counts:?}"
    );
    eprintln!(
        "interaction batch (1200): 1 SELECT; owned-session deletion SELECTs (1/40 children): {deletion_counts:?}"
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn history_reads_overlap_a_writer_and_use_committed_snapshot() {
    let _guard = TEST_LOCK.lock().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let database = SqliteDatabase::open(directory.path().join("concurrent.sqlite"))
        .await
        .unwrap();
    let store = fixture(&database).await;
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let writing = database.clone();
    let writer = tokio::spawn(async move {
        writing
            .transaction(async {
                writing
                    .call(|c| {
                        c.execute(
                            "DELETE FROM ef_history_messages WHERE session_id='owner'",
                            [],
                        )
                    })
                    .await
                    .unwrap();
                ready_tx.send(()).unwrap();
                release_rx.await.unwrap();
                Ok::<_, StateError>(())
            })
            .await
            .unwrap();
    });
    ready_rx.await.unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        store.read_session("owner", HistoryQuery::default()),
    )
    .await;
    release_tx.send(()).unwrap();
    writer.await.unwrap();
    let page = result
        .expect("history read waited for the writer instead of using WAL readers")
        .unwrap();
    assert_eq!(page.records.messages.len(), 60);
    assert!(
        store
            .read_session("owner", HistoryQuery::default())
            .await
            .unwrap()
            .records
            .messages
            .is_empty()
    );
    store.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn event_kind_pages_and_latest_use_native_queries_and_overlap_writer() {
    use eliteforge_agent_test_support::storage_contracts::assert_query_budget;
    use std::time::Duration;
    let _guard = TEST_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db = SqliteDatabase::open(dir.path().join("events.sqlite"))
        .await
        .unwrap();
    let store = SqliteLifecycleStore::with_database(db.clone())
        .await
        .unwrap();
    db.call(|c| -> rusqlite::Result<()> {
        let mut record = SessionRecord::new("events");
        record.id = "events".into();
        c.execute("INSERT INTO ef_session_ids VALUES('events',0)", [])?;
        c.execute(
            "INSERT INTO ef_sessions VALUES('events',NULL,0,'Active',?1)",
            [serde_json::to_string(&record).unwrap()],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    for _ in 0..4 {
        db.call_read(|c| {
            c.trace_v2(
                rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
                Some(count_select),
            );
            Ok::<_, rusqlite::Error>(())
        })
        .await
        .unwrap();
    }
    let mut start = 0;
    for size in [1, 100, 10_000] {
        db.call(move |c| -> rusqlite::Result<()> {
            let tx = c.transaction()?;
            for i in start..size {
                let mut event = SessionEvent::new(
                    "events",
                    if i % 10 == 0 { "target" } else { "noise" },
                    serde_json::json!({"index":i}),
                );
                event.sequence = i + 1;
                tx.execute(
                    "INSERT INTO ef_events VALUES(?1,'events',NULL,NULL,?2,?3,?4)",
                    params![
                        event.id,
                        event.sequence,
                        event.kind,
                        serde_json::to_string(&event).unwrap()
                    ],
                )?;
            }
            tx.commit()
        })
        .await
        .unwrap();
        start = size;
        let before = SELECTS.load(Ordering::SeqCst);
        let page = store
            .list_events_by_kind("events", "target", 0, 3)
            .await
            .unwrap();
        assert_query_budget(
            "filtered event page",
            before,
            SELECTS.load(Ordering::SeqCst),
            1,
        );
        assert_eq!(page.len(), ((size + 9) / 10).min(3) as usize);
        assert!(page.iter().all(|e| e.kind == "target"));
        let before = SELECTS.load(Ordering::SeqCst);
        let latest = store
            .latest_event_by_kind("events", "target")
            .await
            .unwrap()
            .unwrap();
        assert_query_budget("latest event", before, SELECTS.load(Ordering::SeqCst), 1);
        assert_eq!(latest.sequence, ((size - 1) / 10) * 10 + 1);
    }
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let writing = db.clone();
    let writer = tokio::spawn(async move {
        writing
            .call(move |c| {
                let tx = c.transaction()?;
                tx.execute(
                    "UPDATE ef_events SET kind='uncommitted' WHERE session_id='events'",
                    [],
                )?;
                ready_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                tx.rollback()
            })
            .await
    });
    ready_rx.await.unwrap();
    let result = tokio::time::timeout(
        Duration::from_millis(500),
        store.list_events_by_kind("events", "target", 0, 3),
    )
    .await;
    release_tx.send(()).unwrap();
    writer.await.unwrap().unwrap();
    assert_eq!(
        result
            .expect("event reads queued behind writer")
            .unwrap()
            .len(),
        3
    );
}
