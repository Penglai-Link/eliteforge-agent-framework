use async_trait::async_trait;
use eliteforge_agent_core::{Plugin, PluginContext, PluginError, extension_point};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    #[error("operation result is unknown: {0}")]
    Indeterminate(String),
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
    pub fn new() -> Self {
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactMetadata {
    pub id: ArtifactId,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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

#[extension_point(single, adapter = crate::ArtifactStorePlugin)]
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

/// The unique artifact-store service belongs with the storage contract, not a
/// particular database implementation.
pub struct ArtifactStoreService(pub Arc<dyn ArtifactStore>);

pub struct ArtifactStorePlugin(pub Arc<dyn ArtifactStore>);

impl ArtifactStorePlugin {
    pub fn new(store: impl ArtifactStore + 'static) -> Self {
        Self(Arc::new(store))
    }

    pub fn from_arc(store: Arc<dyn ArtifactStore>) -> Self {
        Self(store)
    }
}

#[async_trait]
impl Plugin for ArtifactStorePlugin {
    fn id(&self) -> &str {
        "context.artifact-store"
    }

    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        context.provide(ArtifactStoreService(self.0.clone()))?;
        context.contribute_extension(self.0.clone())
    }
}

impl Default for ArtifactId {
    fn default() -> Self {
        Self::new()
    }
}
