//! Last successfully synchronized model metadata. Credentials and runtime
//! adapters are intentionally absent from the serializable snapshot.
use async_trait::async_trait;
use eliteforge_agent_core::{ModelCatalogSnapshot, ModelCatalogStore, ModelError};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::sync::Semaphore;
use tokio_rusqlite::{
    Connection,
    rusqlite::{self, OptionalExtension, params},
};

const MAX_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub struct SqliteModelCatalogStore {
    connection: Connection,
    admission: Arc<Semaphore>,
}

fn error(_: impl std::fmt::Display) -> ModelError {
    ModelError::Transient("local model catalog operation failed".into())
}

fn validate_scope(scope: &str) -> Result<(), ModelError> {
    if scope.trim().is_empty() || scope.len() > 512 {
        return Err(ModelError::Fatal(
            "invalid model catalog account scope".into(),
        ));
    }
    Ok(())
}

impl SqliteModelCatalogStore {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, ModelError> {
        let connection = Connection::open(path).await.map_err(error)?;
        connection
            .call(|c| -> Result<(), ModelError> {
                c.busy_timeout(Duration::from_secs(5)).map_err(error)?;
                c.execute_batch(
                    "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
                 CREATE TABLE IF NOT EXISTS model_catalog_snapshots (
                     account_scope TEXT PRIMARY KEY, revision INTEGER NOT NULL,
                     synced_at_unix_ms INTEGER NOT NULL, snapshot TEXT NOT NULL
                 );",
                )
                .map_err(error)
            })
            .await
            .map_err(error)?;
        Ok(Self {
            connection,
            admission: Arc::new(Semaphore::new(64)),
        })
    }

    pub async fn close(&self) -> Result<(), ModelError> {
        self.admission.close();
        self.connection.clone().close().await.map_err(error)
    }

    async fn work<T: Send + 'static>(
        &self,
        task: impl FnOnce(&mut rusqlite::Connection) -> Result<T, ModelError> + Send + 'static,
    ) -> Result<T, ModelError> {
        let permit = self
            .admission
            .clone()
            .acquire_owned()
            .await
            .map_err(error)?;
        self.connection
            .call(move |c| {
                let _permit = permit;
                task(c)
            })
            .await
            .map_err(|failure| match failure {
                tokio_rusqlite::Error::Error(error) => error,
                _ => error("worker"),
            })
    }
}

#[async_trait]
impl ModelCatalogStore for SqliteModelCatalogStore {
    async fn load(&self, account_scope: &str) -> Result<Option<ModelCatalogSnapshot>, ModelError> {
        validate_scope(account_scope)?;
        let scope = account_scope.to_owned();
        self.work(move |c| {
            let row: Option<(u64, u64, String)> = c
                .query_row(
                    "SELECT revision,synced_at_unix_ms,snapshot
                 FROM model_catalog_snapshots WHERE account_scope=?1
                   AND length(CAST(snapshot AS BLOB))<=?2",
                    params![scope, MAX_SNAPSHOT_BYTES],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
                .map_err(error)?;
            let Some((revision, synced_at_unix_ms, encoded)) = row else {
                return Ok(None);
            };
            let snapshot: ModelCatalogSnapshot = serde_json::from_str(&encoded)
                .map_err(|_| ModelError::Fatal("stored model catalog is corrupt".into()))?;
            if snapshot.account_scope != scope
                || snapshot.revision != revision
                || snapshot.synced_at_unix_ms != synced_at_unix_ms
            {
                return Err(ModelError::Fatal(
                    "stored model catalog identity mismatch".into(),
                ));
            }
            Ok(Some(snapshot))
        })
        .await
    }

    async fn save(&self, snapshot: &ModelCatalogSnapshot) -> Result<(), ModelError> {
        validate_scope(&snapshot.account_scope)?;
        let revision = i64::try_from(snapshot.revision)
            .map_err(|_| ModelError::Fatal("model catalog revision overflow".into()))?;
        let synced_at_unix_ms = i64::try_from(snapshot.synced_at_unix_ms)
            .map_err(|_| ModelError::Fatal("model catalog timestamp overflow".into()))?;
        let encoded = serde_json::to_string(snapshot)
            .map_err(|_| ModelError::Fatal("model catalog cannot be serialized".into()))?;
        if encoded.len() > MAX_SNAPSHOT_BYTES {
            return Err(ModelError::Fatal(
                "model catalog exceeds storage limit".into(),
            ));
        }
        let scope = snapshot.account_scope.clone();
        self.work(move |c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(error)?;
            let previous: Option<u64> = tx.query_row(
                "SELECT revision FROM model_catalog_snapshots WHERE account_scope=?1",
                [&scope], |row| row.get(0)
            ).optional().map_err(error)?;
            if previous.is_some_and(|previous| previous > revision as u64) {
                return Err(ModelError::Unavailable("stale model catalog revision".into()));
            }
            tx.execute(
                "INSERT INTO model_catalog_snapshots(account_scope,revision,synced_at_unix_ms,snapshot)
                 VALUES (?1,?2,?3,?4)
                 ON CONFLICT(account_scope) DO UPDATE SET
                   revision=excluded.revision,
                   synced_at_unix_ms=excluded.synced_at_unix_ms,
                   snapshot=excluded.snapshot",
                params![scope, revision, synced_at_unix_ms, encoded],
            ).map_err(error)?;
            tx.commit().map_err(error)
        }).await
    }
}

#[async_trait]
impl crate::extensions::SqliteExtensionHost for SqliteModelCatalogStore {
    async fn prepare_extensions(
        &self,
        config: crate::extensions::ExtensionConfig,
    ) -> Result<crate::extensions::ExtensionReport, crate::extensions::ExtensionError> {
        self.work(move |connection| Ok(config.prepare_connection(connection)))
            .await
            .map_err(|_| crate::extensions::ExtensionError::Worker)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn snapshots_survive_restart_and_isolate_accounts() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("catalog.db");
        let store = SqliteModelCatalogStore::open(&path).await.unwrap();
        let snapshot = ModelCatalogSnapshot {
            account_scope: "account-a".into(),
            revision: 3,
            synced_at_unix_ms: 1234,
            entries: vec![],
        };
        store.save(&snapshot).await.unwrap();
        store.close().await.unwrap();

        let reopened = SqliteModelCatalogStore::open(&path).await.unwrap();
        let loaded = reopened.load("account-a").await.unwrap().unwrap();
        assert_eq!(loaded.revision, 3);
        assert_eq!(loaded.synced_at_unix_ms, 1234);
        assert!(reopened.load("account-b").await.unwrap().is_none());

        let mut stale = snapshot;
        stale.revision = 2;
        assert!(matches!(
            reopened.save(&stale).await,
            Err(ModelError::Unavailable(_))
        ));
        assert_eq!(
            reopened.load("account-a").await.unwrap().unwrap().revision,
            3
        );
    }
}
