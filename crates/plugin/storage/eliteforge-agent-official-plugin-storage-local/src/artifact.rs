//! Capability-scoped immutable artifact files.
//!
//! An artifact ID is not an authorization token. Scope stores to the data
//! each agent may access. Untrusted processes must not write to the directory.
use std::{
    collections::HashSet,
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    sync::Arc,
};

use async_trait::async_trait;
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::fs::{Dir, OpenOptions};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const IO_CHUNK: usize = 64 * 1024;
use eliteforge_agent_context_contracts::{
    ArtifactChunk, ArtifactError, ArtifactId, ArtifactMetadata, ArtifactStore,
};

#[derive(Debug, Clone, Copy)]
pub struct FileArtifactLimits {
    pub max_artifact_bytes: usize,
    pub max_read_bytes: usize,
    pub max_in_flight: usize,
}

impl Default for FileArtifactLimits {
    fn default() -> Self {
        Self {
            max_artifact_bytes: 64 * 1024 * 1024,
            max_read_bytes: 64 * 1024,
            max_in_flight: 8,
        }
    }
}

impl FileArtifactLimits {
    fn validate(self) -> Result<Self, ArtifactError> {
        if self.max_artifact_bytes == 0
            || self.max_read_bytes == 0
            || self.max_read_bytes > self.max_artifact_bytes
            || self.max_in_flight == 0
            || self.max_in_flight > Semaphore::MAX_PERMITS
        {
            return Err(ArtifactError::InvalidLimits(
                "positive byte/concurrency limits and read <= artifact are required",
            ));
        }
        Ok(self)
    }
}

/// Uses a directory capability for all artifact operations. Publication uses
/// an atomic, non-overwriting hard link after fsync, then removes the temporary
/// name. A random collision can never overwrite an existing artifact.
#[derive(Clone)]
pub struct FileArtifactStore {
    directory: Arc<Dir>,
    limits: FileArtifactLimits,
    permits: Arc<Semaphore>,
}

impl FileArtifactStore {
    /// The host must create and protect the directory before opening the store.
    pub fn open(path: impl AsRef<Path>, limits: FileArtifactLimits) -> Result<Self, ArtifactError> {
        let limits = limits.validate()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path.as_ref())?.permissions().mode();
            if mode & 0o077 != 0 {
                return Err(ArtifactError::InvalidLimits(
                    "artifact directory must be private to its owner",
                ));
            }
        }
        let directory = Dir::open_ambient_dir(path, cap_std::ambient_authority())?;
        Self::from_dir(directory, limits)
    }

    pub fn from_dir(directory: Dir, limits: FileArtifactLimits) -> Result<Self, ArtifactError> {
        let limits = limits.validate()?;
        Ok(Self {
            directory: Arc::new(directory),
            permits: Arc::new(Semaphore::new(limits.max_in_flight)),
            limits,
        })
    }

    /// Delete unreferenced, aged files. Grace avoids racing a concurrent
    /// process between filesystem publication and its SQLite metadata commit.
    pub(crate) async fn collect_orphans(
        &self,
        referenced: HashSet<String>,
        minimum_age: std::time::Duration,
    ) -> Result<usize, ArtifactError> {
        self.work(CancellationToken::new(), move |directory, _| {
            let mut removed = 0;
            let now = std::time::SystemTime::now();
            for item in directory.entries()? {
                let item = item?;
                let name = item.file_name().to_string_lossy().into_owned();
                if referenced.contains(&name) {
                    continue;
                }
                let is_temporary = name.starts_with('.') && name.ends_with(".tmp");
                if !is_temporary && ArtifactId::parse(&name).is_err() {
                    continue;
                }
                let metadata = item.metadata()?;
                if !metadata.is_file()
                    || now
                        .duration_since(metadata.modified()?.into_std())
                        .unwrap_or_default()
                        < minimum_age
                {
                    continue;
                }
                directory.remove_file(&name)?;
                removed += 1;
            }
            if removed != 0 {
                sync_directory(&directory)?;
            }
            Ok(removed)
        })
        .await
    }

    async fn work<T, F>(&self, cancellation: CancellationToken, work: F) -> Result<T, ArtifactError>
    where
        T: Send + 'static,
        F: FnOnce(Arc<Dir>, CancellationToken) -> Result<T, ArtifactError> + Send + 'static,
    {
        let token = cancellation.child_token();
        let _cancel_on_drop = CancelOnDrop(token.clone());
        let permit = tokio::select! {
            biased;
            _ = token.cancelled() => return Err(ArtifactError::Cancelled),
            permit = self.permits.clone().acquire_owned() => permit
                .map_err(|e| ArtifactError::Worker(e.to_string()))?,
        };
        check_cancelled(&token)?;
        let directory = self.directory.clone();
        // The permit belongs to the worker: dropping the async caller cannot
        // allow unbounded detached blocking work. Drop also cancels its token.
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            work(directory, token)
        })
        .await
        .map_err(|e| ArtifactError::Worker(e.to_string()))?
    }
}

struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

fn check_cancelled(token: &CancellationToken) -> Result<(), ArtifactError> {
    if token.is_cancelled() {
        Err(ArtifactError::Cancelled)
    } else {
        Ok(())
    }
}

struct TemporaryArtifact {
    directory: Arc<Dir>,
    name: String,
}
impl Drop for TemporaryArtifact {
    fn drop(&mut self) {
        let _ = self.directory.remove_file(&self.name);
    }
}

fn sync_directory(directory: &Dir) -> Result<(), ArtifactError> {
    directory.try_clone()?.into_std_file().sync_all()?;
    Ok(())
}

#[async_trait]
impl ArtifactStore for FileArtifactStore {
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
        self.work(cancellation, move |directory, token| {
            check_cancelled(&token)?;
            let name = format!(".{}.tmp", Uuid::new_v4());
            let mut options = OpenOptions::new();
            options
                .write(true)
                .create_new(true)
                .follow(FollowSymlinks::No);
            #[cfg(unix)]
            {
                use cap_std::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = directory.open_with(&name, &options)?;
            let temporary = TemporaryArtifact {
                directory: directory.clone(),
                name,
            };
            for chunk in bytes.chunks(IO_CHUNK) {
                check_cancelled(&token)?;
                file.write_all(chunk)?;
            }
            check_cancelled(&token)?;
            file.sync_all()?;
            drop(file);
            check_cancelled(&token)?;
            let id = ArtifactId::new();
            directory.hard_link(&temporary.name, &directory, id.as_str())?;
            // After publication, finish durability even if cancellation arrives.
            // Callers never receive a partial artifact as a successful result.
            directory.remove_file(&temporary.name)?;
            sync_directory(&directory)?;
            Ok(ArtifactMetadata {
                id,
                size_bytes: bytes.len() as u64,
            })
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
        if length == 0 || length > self.limits.max_read_bytes {
            return Err(ArtifactError::InvalidRange);
        }
        let id = id.clone();
        let maximum = self.limits.max_artifact_bytes as u64;
        self.work(cancellation, move |directory, token| {
            check_cancelled(&token)?;
            let mut options = OpenOptions::new();
            options.read(true).follow(FollowSymlinks::No).nonblock(true);
            let mut file = directory.open_with(id.as_str(), &options)?;
            let metadata = file.metadata()?;
            if !metadata.is_file() {
                return Err(ArtifactError::NotRegularFile);
            }
            let size_bytes = metadata.len();
            if size_bytes > maximum {
                return Err(ArtifactError::TooLarge);
            }
            if offset > size_bytes {
                return Err(ArtifactError::InvalidRange);
            }
            let wanted = length.min((size_bytes - offset) as usize);
            file.seek(SeekFrom::Start(offset))?;
            let mut bytes = vec![0; wanted];
            for chunk in bytes.chunks_mut(IO_CHUNK) {
                check_cancelled(&token)?;
                file.read_exact(chunk)?;
            }
            check_cancelled(&token)?;
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
        self.work(cancellation, move |directory, token| {
            check_cancelled(&token)?;
            directory.remove_file(id.as_str())?;
            sync_directory(&directory)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, FileArtifactStore) {
        let directory = tempfile::tempdir().unwrap();
        let store = FileArtifactStore::open(
            directory.path(),
            FileArtifactLimits {
                max_artifact_bytes: 1024,
                max_read_bytes: 128,
                max_in_flight: 2,
            },
        )
        .unwrap();
        (directory, store)
    }

    #[tokio::test]
    async fn file_publication_is_complete_bounded_and_cancel_safe() {
        let (directory, store) = store();
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(matches!(
            store.put(vec![1], cancelled).await,
            Err(ArtifactError::Cancelled)
        ));
        let data = b"hello world".to_vec();
        let metadata = store
            .put(data.clone(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(directory.path().join(metadata.id.as_str())).unwrap(),
            data
        );
        assert_eq!(
            store
                .read(&metadata.id, 6, 5, CancellationToken::new())
                .await
                .unwrap()
                .bytes,
            b"world"
        );
        assert!(matches!(
            store
                .read(&metadata.id, 0, 129, CancellationToken::new())
                .await,
            Err(ArtifactError::InvalidRange)
        ));
        assert!(matches!(
            store.put(vec![0; 1025], CancellationToken::new()).await,
            Err(ArtifactError::TooLarge)
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinks_and_public_directory_are_rejected() {
        let (directory, store) = store();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), b"secret").unwrap();
        let id = ArtifactId::new();
        std::os::unix::fs::symlink(
            outside.path().join("secret"),
            directory.path().join(id.as_str()),
        )
        .unwrap();
        assert!(
            store
                .read(&id, 0, 1, CancellationToken::new())
                .await
                .is_err()
        );
        assert_eq!(
            std::fs::read(outside.path().join("secret")).unwrap(),
            b"secret"
        );
        let public = directory.path().join("public");
        std::fs::create_dir(&public).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&public, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(FileArtifactStore::open(&public, FileArtifactLimits::default()).is_err());
    }
}
