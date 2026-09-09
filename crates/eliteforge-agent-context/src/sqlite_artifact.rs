//! Immutable SQLite blobs with bounded reads and queued-work admission.
use crate::{
    ArtifactChunk, ArtifactError, ArtifactId, ArtifactMetadata, ArtifactStore, FileArtifactLimits,
};
use async_trait::async_trait;
use std::{path::Path, sync::Arc};
use tokio::sync::Semaphore;
use tokio_rusqlite::{
    Connection,
    rusqlite::{self, OptionalExtension, params},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct SqliteArtifactStore {
    connection: Connection,
    admission: Arc<Semaphore>,
    limits: FileArtifactLimits,
}
fn error(_: impl std::fmt::Display) -> ArtifactError {
    ArtifactError::Worker("SQLite artifact operation failed".into())
}
impl SqliteArtifactStore {
    pub async fn open(
        path: impl AsRef<Path>,
        limits: FileArtifactLimits,
    ) -> Result<Self, ArtifactError> {
        if limits.max_artifact_bytes == 0
            || limits.max_read_bytes == 0
            || limits.max_read_bytes > limits.max_artifact_bytes
            || limits.max_in_flight == 0
            || limits.max_in_flight > Semaphore::MAX_PERMITS
            || limits.max_artifact_bytes > i32::MAX as usize
        {
            return Err(ArtifactError::InvalidLimits(
                "positive bounded blob/read/concurrency limits required",
            ));
        }
        let connection = Connection::open(path).await.map_err(error)?;
        connection.call(|c| -> Result<(),ArtifactError> {
            c.busy_timeout(std::time::Duration::from_secs(5)).map_err(error)?;
            c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS agent_artifacts (id TEXT PRIMARY KEY, data BLOB NOT NULL);").map_err(error)
        }).await.map_err(error)?;
        Ok(Self {
            connection,
            admission: Arc::new(Semaphore::new(limits.max_in_flight)),
            limits,
        })
    }
    pub async fn close(&self) -> Result<(), ArtifactError> {
        self.admission.close();
        self.connection
            .clone()
            .close()
            .await
            .map_err(|_| error("closed"))
    }
    async fn work<T: Send + 'static>(
        &self,
        token: CancellationToken,
        work: impl FnOnce(&mut rusqlite::Connection) -> Result<T, ArtifactError> + Send + 'static,
    ) -> Result<T, ArtifactError> {
        let permit = tokio::select! { biased; _=token.cancelled()=>return Err(ArtifactError::Cancelled), p=self.admission.clone().acquire_owned()=>p.map_err(error)? };
        self.connection
            .call(move |c| {
                let _permit = permit;
                if token.is_cancelled() {
                    return Err(ArtifactError::Cancelled);
                }
                work(c)
            })
            .await
            .map_err(|e| match e {
                tokio_rusqlite::Error::Error(e) => e,
                _ => error("worker"),
            })
    }
}
#[async_trait]
impl ArtifactStore for SqliteArtifactStore {
    fn max_artifact_bytes(&self) -> usize {
        self.limits.max_artifact_bytes
    }
    fn max_read_bytes(&self) -> usize {
        self.limits.max_read_bytes
    }
    async fn put(
        &self,
        bytes: Vec<u8>,
        cancellation: CancellationToken,
    ) -> Result<ArtifactMetadata, ArtifactError> {
        if bytes.len() > self.limits.max_artifact_bytes {
            return Err(ArtifactError::TooLarge);
        }
        self.work(cancellation, move |c| {
            let id = ArtifactId::parse(uuid::Uuid::new_v4().to_string())?;
            let size_bytes = bytes.len() as u64;
            c.execute(
                "INSERT INTO agent_artifacts(id,data) VALUES (?1,?2)",
                params![id.as_str(), bytes],
            )
            .map_err(error)?;
            Ok(ArtifactMetadata { id, size_bytes })
        })
        .await
    }
    async fn read(
        &self,
        id: &ArtifactId,
        offset: u64,
        length: usize,
        cancellation: CancellationToken,
    ) -> Result<ArtifactChunk, ArtifactError> {
        if length == 0 || length > self.limits.max_read_bytes || offset >= i64::MAX as u64 {
            return Err(ArtifactError::InvalidRange);
        }
        let id = id.clone();
        let maximum = self.limits.max_artifact_bytes as u64;
        self.work(cancellation, move |c| {
            let (size_bytes, bytes): (u64, Vec<u8>) = c
                .query_row(
                    "SELECT length(data),substr(data,?2,?3) FROM agent_artifacts WHERE id=?1",
                    params![id.as_str(), offset + 1, length],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(error)?
                .ok_or(ArtifactError::NotFound)?;
            if size_bytes > maximum {
                return Err(ArtifactError::TooLarge);
            }
            if offset > size_bytes {
                return Err(ArtifactError::InvalidRange);
            }
            Ok(ArtifactChunk {
                metadata: ArtifactMetadata { id, size_bytes },
                offset,
                bytes,
            })
        })
        .await
    }
    async fn remove(
        &self,
        id: &ArtifactId,
        cancellation: CancellationToken,
    ) -> Result<(), ArtifactError> {
        let id = id.clone();
        self.work(cancellation, move |c| {
            if c.execute("DELETE FROM agent_artifacts WHERE id=?1", [id.as_str()])
                .map_err(error)?
                == 0
            {
                return Err(ArtifactError::NotFound);
            }
            Ok(())
        })
        .await
    }
}

#[async_trait]
impl eliteforge_agent_sqlite_extensions::SqliteExtensionHost for SqliteArtifactStore {
    async fn prepare_extensions(
        &self,
        config: eliteforge_agent_sqlite_extensions::ExtensionConfig,
    ) -> Result<
        eliteforge_agent_sqlite_extensions::ExtensionReport,
        eliteforge_agent_sqlite_extensions::ExtensionError,
    > {
        self.work(CancellationToken::new(), move |connection| {
            Ok(config.prepare_connection(connection))
        })
        .await
        .map_err(|_| eliteforge_agent_sqlite_extensions::ExtensionError::Worker)?
    }
}
