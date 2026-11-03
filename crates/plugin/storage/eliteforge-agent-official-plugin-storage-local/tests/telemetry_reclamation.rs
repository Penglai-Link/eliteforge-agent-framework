use eliteforge_agent_core::{TelemetryRetention, TelemetryStore};
use eliteforge_agent_official_plugin_storage_local::{SqliteDatabase, SqliteTelemetryStore};

fn retention() -> TelemetryRetention {
    TelemetryRetention {
        trace_max_age: std::time::Duration::from_secs(15 * 86400),
        metric_max_age: std::time::Duration::from_secs(5 * 86400),
        max_completed_traces: Some(50_000),
    }
}

async fn seed(database: &SqliteDatabase, timestamp: i64) {
    database.call(move |connection| -> rusqlite::Result<()> {
        connection.execute_batch("CREATE TABLE host_kept(value TEXT); INSERT INTO host_kept(rowid,value) VALUES(500,'durable');")?;
        let transaction = connection.transaction()?;
        let payload = serde_json::json!({"payload":"x".repeat(64 * 1024)}).to_string();
        for index in 0..128 {
            transaction.execute("INSERT INTO ef_telemetry_metric_points VALUES(?1,'large',?2,?3)", rusqlite::params![format!("point-{index}"), timestamp, payload])?;
        }
        transaction.commit()?;
        connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
    }).await.unwrap();
}

async fn pages(database: &SqliteDatabase) -> (u64, u64, u64) {
    database
        .call(|connection| -> rusqlite::Result<_> {
            Ok((
                connection.query_row("PRAGMA page_count", [], |row| row.get(0))?,
                connection.query_row("PRAGMA freelist_count", [], |row| row.get(0))?,
                connection.query_row("PRAGMA auto_vacuum", [], |row| row.get(0))?,
            ))
        })
        .await
        .unwrap()
}

async fn assert_host_row(database: &SqliteDatabase) {
    database
        .call(|connection| -> rusqlite::Result<()> {
            assert_eq!(
                connection.query_row("SELECT rowid,value FROM host_kept", [], |row| Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?
                )))?,
                (500, "durable".into())
            );
            assert_eq!(
                connection.query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))?,
                "ok"
            );
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn clear_releases_file_and_wal_pages_without_rewriting_host_rowids() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("telemetry.sqlite3");
    let database = SqliteDatabase::open(&path).await.unwrap();
    let store = SqliteTelemetryStore::with_database(database.clone(), retention())
        .await
        .unwrap();
    seed(&database, i64::MAX / 2).await;
    let before = std::fs::metadata(&path).unwrap().len();
    assert!(before > 8 * 1024 * 1024);
    assert_eq!(pages(&database).await.2, 2);
    store.clear().await.unwrap();
    let after = std::fs::metadata(&path).unwrap().len();
    assert!(after < before / 4, "before={before}, after={after}");
    assert_eq!(pages(&database).await.1, 0);
    assert_eq!(
        std::fs::metadata(path.with_extension("sqlite3-wal"))
            .unwrap()
            .len(),
        0
    );
    assert_host_row(&database).await;
    database.close().await.unwrap();
    let reopened = SqliteDatabase::open(&path).await.unwrap();
    assert_eq!(pages(&reopened).await.2, 2);
    assert_host_row(&reopened).await;
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn retention_reclaims_a_bounded_number_of_pages_per_cleanup() {
    let directory = tempfile::tempdir().unwrap();
    let database = SqliteDatabase::open(directory.path().join("retention.sqlite3"))
        .await
        .unwrap();
    let store = SqliteTelemetryStore::with_database(database.clone(), retention())
        .await
        .unwrap();
    seed(&database, 0).await;
    let before = pages(&database).await.0;
    store.cleanup().await.unwrap();
    let first = pages(&database).await;
    assert!(first.0 < before);
    assert!(before - first.0 <= 1030, "one cleanup must remain bounded");
    assert!(
        first.1 > 0,
        "remaining free pages are reclaimed on later passes"
    );
    for _ in 0..4 {
        store.cleanup().await.unwrap();
    }
    assert_eq!(pages(&database).await.1, 0);
    assert_host_row(&database).await;
    database.close().await.unwrap();
}

#[tokio::test]
async fn opening_a_legacy_database_does_not_implicitly_rebuild_it() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("legacy.sqlite3");
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE legacy(value TEXT); INSERT INTO legacy(rowid,value) VALUES(700,'kept');",
        )
        .unwrap();
    drop(connection);
    let database = SqliteDatabase::open(&path).await.unwrap();
    let store = SqliteTelemetryStore::with_database(database.clone(), retention())
        .await
        .unwrap();
    assert_eq!(pages(&database).await.2, 0);
    store.clear().await.unwrap();
    database
        .call(|connection| -> rusqlite::Result<()> {
            assert_eq!(
                connection.query_row("SELECT rowid FROM legacy", [], |row| row.get::<_, i64>(0))?,
                700
            );
            Ok(())
        })
        .await
        .unwrap();
    database.close().await.unwrap();
}
