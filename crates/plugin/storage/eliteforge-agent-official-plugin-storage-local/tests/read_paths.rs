use eliteforge_agent_core::*;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_storage_local::*;
use serde_json::json;

fn session_record(id: &str) -> SessionRecord {
    let mut record = SessionRecord::new("read-paths");
    record.id = id.into();
    record
}

#[tokio::test]
async fn reader_pool_sees_schema_and_rows_committed_after_open() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("read-pool.db");
    let database = SqliteDatabase::open(&path).await.unwrap();
    // Business tables created after the pool was opened must be readable by
    // reader connections, and concurrent reads must not queue on the gate.
    database
        .call(|c| c.execute_batch("CREATE TABLE tron_links(id TEXT PRIMARY KEY)"))
        .await
        .unwrap();
    database
        .call(|c| c.execute("INSERT INTO tron_links VALUES('a')", []))
        .await
        .unwrap();
    let row: String = database
        .call_read(|c| c.query_row("SELECT id FROM tron_links", [], |r| r.get(0)))
        .await
        .unwrap();
    assert_eq!(row, "a");
    let readers: Vec<_> = (0..8)
        .map(|i| {
            let database = database.clone();
            tokio::spawn(async move {
                database
                    .call_read(move |c| {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        c.query_row("SELECT count(*) FROM tron_links", [], |r| {
                            r.get::<_, i64>(0)
                        })
                    })
                    .await
                    .map(|count| (i, count))
            })
        })
        .collect();
    let started = std::time::Instant::now();
    let mut counts = Vec::new();
    for reader in readers {
        counts.push(reader.await.unwrap().unwrap());
    }
    assert!(counts.into_iter().all(|(_, count)| count == 1));
    // Serialized reads would take at least 8 * 20ms; pooled readers overlap.
    assert!(started.elapsed() < std::time::Duration::from_millis(160));
}

#[tokio::test]
async fn call_read_inside_a_transaction_joins_the_write_connection() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("transactional-read.db");
    let database = SqliteDatabase::open(&path).await.unwrap();
    database
        .call(|c| c.execute_batch("CREATE TABLE tron_links(id TEXT PRIMARY KEY)"))
        .await
        .unwrap();
    database
        .transaction(async {
            database
                .call(|c| c.execute("INSERT INTO tron_links VALUES('uncommitted')", []))
                .await
                .map_err(|_| StateError::Backend("insert failed".into()))?;
            let row: Option<String> = database
                .call_read(|c| {
                    use rusqlite::OptionalExtension;
                    c.query_row("SELECT id FROM tron_links", [], |r| r.get(0))
                        .optional()
                })
                .await
                .map_err(|_| StateError::Backend("read failed".into()))?;
            assert_eq!(row.as_deref(), Some("uncommitted"));
            Err::<(), eliteforge_agent_core::StateError>(StateError::Conflict)
        })
        .await
        .unwrap_err();
    let remaining: i64 = database
        .call_read(|c| c.query_row("SELECT count(*) FROM tron_links", [], |r| r.get(0)))
        .await
        .unwrap();
    assert_eq!(remaining, 0);
}#[tokio::test]
async fn memory_database_reads_fall_back_to_the_shared_connection() {
    let database = SqliteDatabase::open(":memory:").await.unwrap();
    database
        .call(|c| c.execute_batch("CREATE TABLE tron_links(id TEXT PRIMARY KEY); INSERT INTO tron_links VALUES('a')"))
        .await
        .unwrap();
    let row: String = database
        .call_read(|c| c.query_row("SELECT id FROM tron_links", [], |r| r.get(0)))
        .await
        .unwrap();
    assert_eq!(row, "a");
}

#[tokio::test]
async fn events_filter_by_kind_in_sql() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("event-kind.db");
    let artifacts = directory.path().join("artifacts");
    std::fs::create_dir(&artifacts).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&artifacts, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let framework = Framework::builder()
        .default_models(false)
        .plugin(LocalStoragePlugin::new(&path, &artifacts))
        .build()
        .await
        .unwrap();
    let sessions = framework.sessions().unwrap();
    sessions.create(session_record("kind-session")).await.unwrap();
    let store = framework.events().unwrap();
    for kind in ["alpha", "beta", "alpha"] {
        store
            .0
            .append_event(SessionEvent::new("kind-session", kind, json!({"kind": kind})))
            .await
            .unwrap();
    }
    let events = store
        .list_by_kind("kind-session", "alpha", 0, 1000)
        .await
        .unwrap();
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|event| event.kind == "alpha"));
    let paged = store
        .list_by_kind("kind-session", "alpha", 0, 1)
        .await
        .unwrap();
    assert_eq!(paged.len(), 1);
    let after_first = store
        .list_by_kind("kind-session", "alpha", paged[0].sequence, 1)
        .await
        .unwrap();
    assert_eq!(after_first.len(), 1);
    assert_ne!(after_first[0].id, paged[0].id);
}
