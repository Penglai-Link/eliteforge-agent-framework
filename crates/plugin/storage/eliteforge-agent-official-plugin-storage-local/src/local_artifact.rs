//! SQLite metadata plus filesystem payloads. The legacy agent_artifacts BLOB
//! table is deliberately ignored: its IDs have no local metadata rows.
use crate::{ArtifactChunk, ArtifactError, ArtifactId, ArtifactMetadata, ArtifactStore, FileArtifactStore};
use async_trait::async_trait;
use std::{collections::HashSet, path::Path, sync::Arc, time::Duration};
use tokio::sync::Semaphore;
use tokio_rusqlite::{Connection, rusqlite::{self, OptionalExtension, params}};
use tokio_util::sync::CancellationToken;

const ORPHAN_GRACE: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone)]
pub struct LocalArtifactStore {
    connection: Connection,
    admission: Arc<Semaphore>,
    files: FileArtifactStore,
}

fn error(_: impl std::fmt::Display) -> ArtifactError {
    ArtifactError::Worker("local artifact metadata operation failed".into())
}

impl LocalArtifactStore {
    pub async fn open(path: impl AsRef<Path>, files: FileArtifactStore) -> Result<Self, ArtifactError> {
        let connection = Connection::open(path).await.map_err(error)?;
        connection.call(|c| -> Result<(), ArtifactError> {
            c.busy_timeout(Duration::from_secs(5)).map_err(error)?;
            c.execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
                 CREATE TABLE IF NOT EXISTS local_artifact_metadata (
                     id TEXT PRIMARY KEY, size_bytes INTEGER NOT NULL CHECK(size_bytes >= 0),
                     created_at_unix_ms INTEGER NOT NULL
                 );"
            ).map_err(error)
        }).await.map_err(error)?;
        let store = Self {
            connection,
            admission: Arc::new(Semaphore::new(64)),
            files,
        };
        store.collect_orphans().await?;
        Ok(store)
    }

    pub async fn close(&self) -> Result<(), ArtifactError> {
        self.admission.close();
        self.connection.clone().close().await.map_err(error)
    }

    async fn work<T: Send + 'static>(
        &self,
        task: impl FnOnce(&mut rusqlite::Connection) -> Result<T, ArtifactError> + Send + 'static,
    ) -> Result<T, ArtifactError> {
        let permit = self.admission.clone().acquire_owned().await.map_err(error)?;
        self.connection.call(move |c| {
            let _permit = permit;
            task(c)
        }).await.map_err(|failure| match failure {
            tokio_rusqlite::Error::Error(error) => error,
            _ => error("worker"),
        })
    }

    async fn metadata(&self, id: &ArtifactId) -> Result<ArtifactMetadata, ArtifactError> {
        let id = id.clone();
        self.work(move |c| {
            let size_bytes: Option<u64> = c.query_row(
                "SELECT size_bytes FROM local_artifact_metadata WHERE id=?1",
                [id.as_str()], |row| row.get(0)
            ).optional().map_err(error)?;
            Ok(ArtifactMetadata { id, size_bytes: size_bytes.ok_or(ArtifactError::NotFound)? })
        }).await
    }

    /// Sweep only files older than one day, leaving current in-flight writes
    /// and another process's publication window intact.
    pub async fn collect_orphans(&self) -> Result<usize, ArtifactError> {
        self.collect_orphans_older_than(ORPHAN_GRACE).await
    }

    async fn collect_orphans_older_than(&self, minimum_age: Duration) -> Result<usize, ArtifactError> {
        let referenced = self.work(|c| {
            let mut stmt = c.prepare("SELECT id FROM local_artifact_metadata").map_err(error)?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0)).map_err(error)?;
            rows.collect::<rusqlite::Result<HashSet<String>>>().map_err(error)
        }).await?;
        self.files.collect_orphans(referenced, minimum_age).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FileArtifactLimits;

    async fn store() -> (tempfile::TempDir, LocalArtifactStore) {
        let directory = tempfile::tempdir().unwrap();
        let files = FileArtifactStore::open(directory.path(), FileArtifactLimits::default()).unwrap();
        let store = LocalArtifactStore::open(directory.path().join("agent.db"), files).await.unwrap();
        (directory, store)
    }

    #[tokio::test]
    async fn metadata_controls_visibility_across_reopen() {
        let (directory, store) = store().await;
        let bytes = vec![0, 1, 255, 0, 42];
        let metadata = store.put(bytes.clone(), CancellationToken::new()).await.unwrap();
        store.close().await.unwrap();
        let files = FileArtifactStore::open(directory.path(), FileArtifactLimits::default()).unwrap();
        let reopened = LocalArtifactStore::open(directory.path().join("agent.db"), files).await.unwrap();
        assert_eq!(
            reopened.read(&metadata.id, 1, 3, CancellationToken::new()).await.unwrap().bytes,
            bytes[1..4]
        );
        reopened.remove(&metadata.id, CancellationToken::new()).await.unwrap();
        assert!(matches!(
            reopened.read(&metadata.id, 0, 1, CancellationToken::new()).await,
            Err(ArtifactError::NotFound)
        ));
    }

    #[tokio::test]
    async fn legacy_blob_id_is_not_read_and_orphans_are_collected() {
        let (directory, store) = store().await;
        let legacy = ArtifactId::new();
        let db = Connection::open(directory.path().join("agent.db")).await.unwrap();
        let key = legacy.as_str().to_owned();
        db.call(move |c| -> rusqlite::Result<()> {
            c.execute_batch("CREATE TABLE agent_artifacts(id TEXT PRIMARY KEY, data BLOB NOT NULL)")?;
            c.execute("INSERT INTO agent_artifacts(id,data) VALUES (?1,?2)", params![key, vec![1u8]])?;
            Ok(())
        }).await.unwrap();
        std::fs::write(directory.path().join(legacy.as_str()), b"legacy").unwrap();
        assert!(matches!(
            store.read(&legacy, 0, 1, CancellationToken::new()).await,
            Err(ArtifactError::NotFound)
        ));
        assert_eq!(store.collect_orphans_older_than(Duration::ZERO).await.unwrap(), 1);
        assert!(!directory.path().join(legacy.as_str()).exists());
        db.close().await.unwrap();
    }

    #[tokio::test]
    async fn metadata_failure_rolls_back_published_file() {
        let (directory, store) = store().await;
        let db = Connection::open(directory.path().join("agent.db")).await.unwrap();
        db.call(|c| -> rusqlite::Result<()> {
            c.execute_batch("DROP TABLE local_artifact_metadata")
        }).await.unwrap();
        assert!(store.put(vec![1, 2], CancellationToken::new()).await.is_err());
        let file_count = std::fs::read_dir(directory.path()).unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().parse::<uuid::Uuid>().is_ok())
            .count();
        assert_eq!(file_count, 0);
        db.close().await.unwrap();
    }
}

#[async_trait]
impl ArtifactStore for LocalArtifactStore {
    fn max_artifact_bytes(&self) -> usize { self.files.max_artifact_bytes() }
    fn max_read_bytes(&self) -> usize { self.files.max_read_bytes() }

    async fn put(&self, bytes: Vec<u8>, cancellation: CancellationToken) -> Result<ArtifactMetadata, ArtifactError> {
        let metadata = self.files.put(bytes, cancellation).await?;
        let id = metadata.id.clone();
        let size_bytes = metadata.size_bytes;
        let recorded = self.work(move |c| {
            c.execute(
                "INSERT INTO local_artifact_metadata(id,size_bytes,created_at_unix_ms)
                 VALUES (?1,?2,CAST(unixepoch('subsec') * 1000 AS INTEGER))",
                params![id.as_str(), size_bytes],
            ).map_err(error)?;
            Ok(())
        }).await;
        if recorded.is_err() {
            // Best effort rollback. A crash here leaves an aged orphan for GC.
            let _ = self.files.remove(&metadata.id, CancellationToken::new()).await;
        }
        recorded.map(|()| metadata)
    }

    async fn read(
        &self, id: &ArtifactId, offset: u64, length: usize,
        cancellation: CancellationToken,
    ) -> Result<ArtifactChunk, ArtifactError> {
        let metadata = self.metadata(id).await?;
        let chunk = self.files.read(id, offset, length, cancellation).await?;
        if chunk.metadata.size_bytes != metadata.size_bytes {
            return Err(ArtifactError::Worker("artifact metadata/file size mismatch".into()));
        }
        Ok(chunk)
    }

    async fn remove(&self, id: &ArtifactId, cancellation: CancellationToken) -> Result<(), ArtifactError> {
        if cancellation.is_cancelled() {
            return Err(ArtifactError::Cancelled);
        }
        let id = id.clone();
        self.work(move |c| {
            if c.execute("DELETE FROM local_artifact_metadata WHERE id=?1", [id.as_str()]).map_err(error)? == 0 {
                return Err(ArtifactError::NotFound);
            }
            Ok(())
        }).await?;
        // The metadata row is the visibility boundary. Failure to unlink is
        // cleaned by the delayed orphan collector.
        match self.files.remove(&id, CancellationToken::new()).await {
            Ok(()) => Ok(()),
            Err(ArtifactError::NotFound) => Ok(()),
            Err(_) => Err(ArtifactError::Indeterminate(
                "metadata removed; filesystem cleanup pending".into(),
            )),
        }
    }
}
