//! Capability-scoped artifacts and lossless tool-output offloading.
//!
//! An artifact ID is not an authorization token. Give each reader/offloader a
//! store scoped to the data its agent may access. The directory belongs to the
//! store: untrusted processes must not have write access to it.
use std::{
    collections::HashSet,
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    sync::Arc,
};

use agent_framework::{
    Content, ContentBlock, ResourceReference, RunContext, Tool, ToolBinding, ToolDescriptor,
    ToolError, ToolInvocationContext, ToolMiddleware, ToolNext, ToolOutput,
};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::fs::{Dir, OpenOptions};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const IO_CHUNK: usize = 64 * 1024;
const TOOL_OUTPUT_MIME: &str = "application/vnd.agent-framework.tool-output+json";

#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    #[error("invalid artifact identifier")]
    InvalidId,
    #[error("artifact does not exist in this store or has expired")]
    NotFound,
    #[error("invalid artifact limits: {0}")]
    InvalidLimits(&'static str),
    #[error("artifact exceeds the configured byte limit")]
    TooLarge,
    #[error("invalid artifact read range")]
    InvalidRange,
    #[error("artifact operation cancelled")]
    Cancelled,
    #[error("artifact is not a regular file")]
    NotRegularFile,
    #[error("artifact I/O failed: {0}")]
    Io(#[source] std::io::Error),
    #[error("artifact worker failed: {0}")]
    Worker(String),
    #[error("tool output serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}

impl From<std::io::Error> for ArtifactError {
    fn from(error: std::io::Error) -> Self {
        if error.kind() == std::io::ErrorKind::NotFound {
            Self::NotFound
        } else {
            Self::Io(error)
        }
    }
}

/// Only canonical, lowercase, randomly generated UUIDs can name artifacts.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ArtifactId(String);

impl ArtifactId {
    pub fn parse(value: impl Into<String>) -> Result<Self, ArtifactError> {
        let value = value.into();
        let uuid = Uuid::parse_str(&value).map_err(|_| ArtifactError::InvalidId)?;
        if uuid.get_version_num() != 4 || uuid.to_string() != value {
            return Err(ArtifactError::InvalidId);
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }
}

impl TryFrom<String> for ArtifactId {
    type Error = ArtifactError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}
impl From<ArtifactId> for String {
    fn from(value: ArtifactId) -> Self {
        value.0
    }
}
impl std::fmt::Display for ArtifactId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactMetadata {
    pub id: ArtifactId,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactChunk {
    pub metadata: ArtifactMetadata,
    pub offset: u64,
    pub bytes: Vec<u8>,
}

impl ArtifactChunk {
    pub fn next_offset(&self) -> u64 {
        self.offset + self.bytes.len() as u64
    }
    pub fn eof(&self) -> bool {
        self.next_offset() == self.metadata.size_bytes
    }
}

#[async_trait]
pub trait ArtifactStore: Send + Sync {
    fn max_artifact_bytes(&self) -> usize;
    fn max_read_bytes(&self) -> usize;
    /// Publish immutable bytes atomically. Success means the full data exists.
    async fn put(
        &self,
        bytes: Vec<u8>,
        cancellation: CancellationToken,
    ) -> Result<ArtifactMetadata, ArtifactError>;
    /// Byte ranges may split UTF-8 codepoints; callers must preserve the bytes.
    async fn read(
        &self,
        id: &ArtifactId,
        offset: u64,
        length: usize,
        cancellation: CancellationToken,
    ) -> Result<ArtifactChunk, ArtifactError>;
    /// Host-managed retention must only delete unreferenced artifacts.
    async fn remove(
        &self,
        id: &ArtifactId,
        cancellation: CancellationToken,
    ) -> Result<(), ArtifactError>;
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

/// Offloads content and structured data. Error and control fields remain inline.
/// Add the artifact reader's tool ID to exclusions when its configured range
/// could itself exceed the offload threshold.
pub struct ToolOutputOffloader {
    store: Arc<dyn ArtifactStore>,
    threshold_bytes: usize,
    preview_bytes: usize,
    excluded_tools: HashSet<String>,
}

impl ToolOutputOffloader {
    pub fn new(
        store: Arc<dyn ArtifactStore>,
        threshold_bytes: usize,
        preview_bytes: usize,
    ) -> Result<Self, ArtifactError> {
        if threshold_bytes == 0 || preview_bytes > threshold_bytes {
            return Err(ArtifactError::InvalidLimits(
                "offload threshold must be positive and preview <= threshold",
            ));
        }
        Ok(Self {
            store,
            threshold_bytes,
            preview_bytes,
            excluded_tools: HashSet::new(),
        })
    }

    pub fn excluding_tool(mut self, tool_id: impl Into<String>) -> Self {
        self.excluded_tools.insert(tool_id.into());
        self
    }

    pub async fn offload(
        &self,
        output: &mut ToolOutput,
        cancellation: CancellationToken,
    ) -> Result<Option<ArtifactMetadata>, ArtifactError> {
        check_cancelled(&cancellation)?;
        let mut counter = BoundedBuffer::counting(self.threshold_bytes);
        if serde_json::to_writer(&mut counter, &(&output.content, &output.structured)).is_ok() {
            return Ok(None);
        }
        let mut buffer = BoundedBuffer::collecting(self.store.max_artifact_bytes());
        if let Err(error) = serde_json::to_writer(&mut buffer, &*output) {
            return if buffer.exceeded {
                Err(ArtifactError::TooLarge)
            } else {
                Err(ArtifactError::Serialization(error))
            };
        }
        let preview = content_preview(&output.content, self.preview_bytes);
        // Original content and runtime semantics remain unchanged on failure.
        let metadata = self.store.put(buffer.bytes, cancellation).await?;
        let reference = ResourceReference {
            uri: format!("artifact:{}", metadata.id),
            mime_type: Some(TOOL_OUTPUT_MIME.into()),
            name: Some("Original tool output".into()),
            size_bytes: Some(metadata.size_bytes),
        };
        output.structured = None;
        output.result_reference = Some(Box::new(reference.clone()));
        output.content = vec![
            ContentBlock::Text {
                text: format!(
                    "Tool output stored as artifact {} ({} bytes). Preview:\n{}",
                    metadata.id, metadata.size_bytes, preview
                ),
            },
            ContentBlock::Resource {
                resource: reference,
            },
        ]
        .into();
        Ok(Some(metadata))
    }
}

fn content_preview(content: &Content, limit: usize) -> String {
    let mut preview = String::new();
    for block in &content.0 {
        if let ContentBlock::Text { text } = block {
            let mut length = text.len().min(limit - preview.len());
            while !text.is_char_boundary(length) {
                length -= 1;
            }
            preview.push_str(&text[..length]);
            if length < text.len() || preview.len() == limit {
                break;
            }
        }
    }
    preview
}

struct BoundedBuffer {
    bytes: Vec<u8>,
    size: usize,
    limit: usize,
    collect: bool,
    exceeded: bool,
}
impl BoundedBuffer {
    fn counting(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            size: 0,
            limit,
            collect: false,
            exceeded: false,
        }
    }
    fn collecting(limit: usize) -> Self {
        Self {
            collect: true,
            ..Self::counting(limit)
        }
    }
}
impl Write for BoundedBuffer {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if data.len() > self.limit - self.size {
            self.exceeded = true;
            return Err(std::io::Error::other("artifact byte limit exceeded"));
        }
        self.size += data.len();
        if self.collect {
            self.bytes.extend_from_slice(data);
        }
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl ToolMiddleware for ToolOutputOffloader {
    async fn intercept(
        &self,
        context: &ToolInvocationContext<'_>,
        next: ToolNext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let mut output = next.run().await?;
        if self.excluded_tools.contains(&context.descriptor.id) {
            return Ok(output);
        }
        {
            let token = CancellationToken::new();
            let _cancel_on_drop = CancelOnDrop(token.clone());
            tokio::select! {
                biased;
                _ = context.run.cancelled() => {
                    token.cancel();
                    return Err(ToolError::Failed("artifact offload cancelled".into()));
                }
                result = self.offload(&mut output, token.clone()) => {
                    result.map_err(|e| ToolError::Failed(e.to_string()))?;
                }
            }
        }
        Ok(output)
    }
}

/// Reads raw artifact bytes in bounded pages. UTF-8 pages are returned as text;
/// pages splitting a codepoint or containing binary data use base64 losslessly.
#[derive(Clone)]
pub struct ArtifactReadTool {
    store: Arc<dyn ArtifactStore>,
}
impl ArtifactReadTool {
    pub fn new(store: Arc<dyn ArtifactStore>) -> Self {
        Self { store }
    }

    pub fn binding(self, id: impl Into<String>, call_name: impl Into<String>) -> ToolBinding {
        let max_read_bytes = self.store.max_read_bytes();
        ToolBinding::new(
            ToolDescriptor::new(
                id,
                call_name,
                "Read a bounded byte range from an authorized artifact. Use next_offset to continue; encoding is utf8 or base64.",
                json!({
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "pattern": "^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$"},
                        "offset": {"type": "integer", "minimum": 0},
                        "length": {"type": "integer", "minimum": 1, "maximum": max_read_bytes}
                    },
                    "required": ["id", "length"],
                    "additionalProperties": false
                }),
            ),
            self,
        )
        .parallel_safe()
    }

    async fn read_arguments(
        &self,
        arguments: Value,
        token: CancellationToken,
    ) -> Result<ToolOutput, ArtifactError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Arguments {
            id: ArtifactId,
            #[serde(default)]
            offset: u64,
            length: usize,
        }
        let arguments: Arguments = serde_json::from_value(arguments)?;
        let chunk = self
            .store
            .read(&arguments.id, arguments.offset, arguments.length, token)
            .await?;
        let (encoding, data) = match std::str::from_utf8(&chunk.bytes) {
            Ok(text) => ("utf8", text.to_owned()),
            Err(_) => ("base64", STANDARD.encode(&chunk.bytes)),
        };
        let value = json!({
            "id": chunk.metadata.id,
            "size_bytes": chunk.metadata.size_bytes,
            "offset": chunk.offset,
            "next_offset": chunk.next_offset(),
            "eof": chunk.eof(),
            "encoding": encoding,
            "data": data,
        });
        Ok(ToolOutput {
            structured: Some(value.clone()),
            content: Content(vec![ContentBlock::Json { value }]),
            ..ToolOutput::default()
        })
    }
}

#[async_trait]
impl Tool for ArtifactReadTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let token = CancellationToken::new();
        let _cancel_on_drop = CancelOnDrop(token.clone());
        tokio::select! {
            biased;
            _ = context.cancelled() => {
                token.cancel();
                Err(ToolError::Failed("artifact read cancelled".into()))
            }
            result = self.read_arguments(arguments, token.clone()) => result.map_err(|e| ToolError::Failed(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_framework::ToolEffect;

    fn store() -> (tempfile::TempDir, Arc<FileArtifactStore>) {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(
            FileArtifactStore::open(
                directory.path(),
                FileArtifactLimits {
                    max_artifact_bytes: 1024 * 1024,
                    max_read_bytes: 4096,
                    max_in_flight: 2,
                },
            )
            .unwrap(),
        );
        (directory, store)
    }

    #[test]
    fn identifiers_reject_paths_noncanonical_and_nonrandom_uuids() {
        for value in [
            "../secret",
            "/etc/passwd",
            "a/b",
            ".",
            "",
            "urn:uuid:8a6d6b78-cc24-40ef-94cd-c960dbb7b293",
            "8A6D6B78-CC24-40EF-94CD-C960DBB7B293",
            "00000000-0000-0000-0000-000000000000",
        ] {
            assert!(ArtifactId::parse(value).is_err(), "{value}");
            assert!(serde_json::from_value::<ArtifactId>(json!(value)).is_err());
        }
        let id = ArtifactId::new();
        assert_eq!(serde_json::from_value::<ArtifactId>(json!(id)).unwrap(), id);
    }

    #[tokio::test]
    async fn files_are_published_complete_and_reopen_with_bounded_ranges() {
        let (directory, store) = store();
        let data = "hello 世界".as_bytes().to_vec();
        let metadata = store
            .put(data.clone(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(metadata.size_bytes, data.len() as u64);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        let reopened = FileArtifactStore::open(directory.path(), store.limits).unwrap();
        let chunk = reopened
            .read(&metadata.id, 6, 3, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(chunk.bytes, "世".as_bytes());
        assert_eq!(chunk.next_offset(), 9);
        assert!(!chunk.eof());
        let last = reopened
            .read(&metadata.id, 9, 4096, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(last.bytes, "界".as_bytes());
        assert!(last.eof());
        let empty = reopened
            .read(
                &metadata.id,
                metadata.size_bytes,
                1,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(empty.bytes.is_empty() && empty.eof());
        reopened
            .remove(&metadata.id, CancellationToken::new())
            .await
            .unwrap();
        assert!(
            reopened
                .read(&metadata.id, 0, 1, CancellationToken::new())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn store_enforces_size_and_read_limits() {
        let (directory, store) = store();
        assert!(matches!(
            store
                .put(
                    vec![0; store.max_artifact_bytes() + 1],
                    CancellationToken::new()
                )
                .await,
            Err(ArtifactError::TooLarge)
        ));
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        let metadata = store
            .put(vec![1, 2, 3], CancellationToken::new())
            .await
            .unwrap();
        for (offset, length) in [(0, 0), (0, 4097), (4, 1), (u64::MAX, 1)] {
            assert!(matches!(
                store
                    .read(&metadata.id, offset, length, CancellationToken::new())
                    .await,
                Err(ArtifactError::InvalidRange)
            ));
        }
        std::fs::write(
            directory.path().join(metadata.id.as_str()),
            vec![0; store.max_artifact_bytes() + 1],
        )
        .unwrap();
        assert!(matches!(
            store
                .read(&metadata.id, 0, 1, CancellationToken::new())
                .await,
            Err(ArtifactError::TooLarge)
        ));
    }

    #[tokio::test]
    async fn cancellation_prevents_writes_reads_and_deletes() {
        let (directory, store) = store();
        let token = CancellationToken::new();
        token.cancel();
        assert!(matches!(
            store.put(vec![1], token.clone()).await,
            Err(ArtifactError::Cancelled)
        ));
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        let metadata = store.put(vec![1], CancellationToken::new()).await.unwrap();
        assert!(matches!(
            store.read(&metadata.id, 0, 1, token.clone()).await,
            Err(ArtifactError::Cancelled)
        ));
        assert!(matches!(
            store.remove(&metadata.id, token).await,
            Err(ArtifactError::Cancelled)
        ));
        assert!(directory.path().join(metadata.id.as_str()).exists());
    }

    #[tokio::test]
    async fn cancellation_while_waiting_for_capacity_does_not_start_work() {
        let (directory, store) = store();
        let _permits = store.permits.acquire_many(2).await.unwrap();
        let token = CancellationToken::new();
        let pending = store.put(vec![1], token.clone());
        tokio::pin!(pending);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut pending)
                .await
                .is_err()
        );
        token.cancel();
        assert!(matches!(pending.await, Err(ArtifactError::Cancelled)));
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinks_inside_and_outside_directory_are_never_followed() {
        let (directory, store) = store();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), b"secret").unwrap();
        let original = store
            .put(b"inside".to_vec(), CancellationToken::new())
            .await
            .unwrap();
        for target in [
            outside.path().join("secret"),
            std::path::PathBuf::from(original.id.as_str()),
        ] {
            let id = ArtifactId::new();
            std::os::unix::fs::symlink(target, directory.path().join(id.as_str())).unwrap();
            assert!(
                store
                    .read(&id, 0, 20, CancellationToken::new())
                    .await
                    .is_err()
            );
        }
        assert_eq!(
            std::fs::read(outside.path().join("secret")).unwrap(),
            b"secret"
        );
        let id = ArtifactId::new();
        std::fs::create_dir(directory.path().join(id.as_str())).unwrap();
        assert!(
            store
                .read(&id, 0, 1, CancellationToken::new())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn offload_preserves_full_output_and_runtime_semantics() {
        let (_directory, store) = store();
        let offloader = ToolOutputOffloader::new(store.clone(), 128, 5).unwrap();
        let original = ToolOutput {
            content: format!("世界{}", "x".repeat(500)).into(),
            structured: Some(json!({"important": true})),
            is_error: true,
            effect: ToolEffect::ExposeTools(vec!["dynamic".into()]),
            result_reference: None,
        };
        let mut output = original.clone();
        let metadata = offloader
            .offload(&mut output, CancellationToken::new())
            .await
            .unwrap()
            .unwrap();
        assert!(output.structured.is_none());
        assert!(output.result_reference.is_some());
        assert_eq!(output.is_error, original.is_error);
        assert_eq!(output.effect, original.effect);
        assert!(output.content.text().ends_with("\n世"));
        let chunk = store
            .read(&metadata.id, 0, 4096, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<ToolOutput>(&chunk.bytes).unwrap(),
            original
        );
        assert!(
            matches!(&output.content.0[1], ContentBlock::Resource { resource } if resource.uri == format!("artifact:{}", metadata.id))
        );
    }

    #[tokio::test]
    async fn failed_and_unnecessary_offloads_leave_original_unchanged() {
        let (directory, store) = store();
        let offloader = ToolOutputOffloader::new(store.clone(), 128, 8).unwrap();
        let small = ToolOutput::text("short");
        let mut output = small.clone();
        assert_eq!(
            offloader
                .offload(&mut output, CancellationToken::new())
                .await
                .unwrap(),
            None
        );
        assert_eq!(output, small);
        let mut output = ToolOutput::text("x".repeat(store.max_artifact_bytes()));
        let original = output.clone();
        assert!(matches!(
            offloader
                .offload(&mut output, CancellationToken::new())
                .await,
            Err(ArtifactError::TooLarge)
        ));
        assert_eq!(output, original);
        let token = CancellationToken::new();
        token.cancel();
        assert!(matches!(
            offloader.offload(&mut output, token).await,
            Err(ArtifactError::Cancelled)
        ));
        assert_eq!(output, original);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn reader_preserves_split_utf8_bytes_and_validates_arguments() {
        let (_directory, store) = store();
        let metadata = store
            .put("世界".as_bytes().to_vec(), CancellationToken::new())
            .await
            .unwrap();
        let reader = ArtifactReadTool::new(store);
        let output = reader
            .read_arguments(
                json!({"id": metadata.id, "offset": 1, "length": 2}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let value = output.structured.unwrap();
        assert_eq!(value["encoding"], "base64");
        assert_eq!(
            STANDARD.decode(value["data"].as_str().unwrap()).unwrap(),
            "世界".as_bytes()[1..3]
        );
        assert_eq!(value["next_offset"], 3);
        assert_eq!(value["eof"], false);
        for arguments in [
            json!({"id": "../secret", "length": 1}),
            json!({"id": metadata.id, "length": 0}),
            json!({"id": metadata.id, "length": 4097}),
            json!({"id": metadata.id, "length": 1, "path": "/etc/passwd"}),
        ] {
            assert!(
                reader
                    .read_arguments(arguments, CancellationToken::new())
                    .await
                    .is_err()
            );
        }
    }

    #[test]
    fn preview_is_bounded_across_blocks_and_utf8() {
        let content = Content::from(vec![
            ContentBlock::Text { text: "a".into() },
            ContentBlock::Json {
                value: json!({"skip": true}),
            },
            ContentBlock::Text {
                text: "世界".into(),
            },
        ]);
        assert_eq!(content_preview(&content, 0), "");
        assert_eq!(content_preview(&content, 3), "a");
        assert_eq!(content_preview(&content, 4), "a世");
    }
}
