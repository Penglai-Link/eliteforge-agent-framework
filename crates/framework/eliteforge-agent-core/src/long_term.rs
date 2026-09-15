//! Cross-conversation records. Namespaces are opaque host-resolved access scopes;
//! neither model arguments nor framework business enums select a namespace.
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemorySource {
    pub id: String,
    pub version: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryWrite {
    /// Stable caller-chosen ID. Create rejects duplicate IDs; corrections require
    /// the current version. Similar content under different IDs is not merged.
    pub id: String,
    pub content: String,
    pub source: MemorySource,
    /// Unix milliseconds. Expired records are absent from reads and searches.
    pub expires_at_ms: Option<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryEntry {
    pub namespace: String,
    /// Opaque identity of this version, never reused after deletion/recreation.
    pub version: String,
    pub updated_at_ms: u64,
    pub value: MemoryWrite,
}
#[derive(Debug, Clone)]
pub struct MemoryQuery {
    pub text: String,
    pub limit: usize,
    /// Bounds the sum of serialized entry bytes returned by a backend.
    pub max_bytes: usize,
}
impl MemoryQuery {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            limit: 20,
            max_bytes: 64 * 1024,
        }
    }
}
#[derive(Debug, Clone, thiserror::Error)]
pub enum MemoryError {
    #[error("memory entry does not exist or has expired")]
    NotFound,
    #[error("memory version conflict")]
    Conflict,
    #[error("invalid memory request: {0}")]
    Invalid(String),
    #[error("memory storage limit exceeded")]
    Capacity,
    #[error("memory backend failed: {0}")]
    Backend(String),
}
#[crate::extension_point(single, adapter = crate::LongTermMemoryPlugin)]
#[async_trait]
pub trait LongTermMemory: Send + Sync {
    async fn get(&self, namespace: &str, id: &str) -> Result<MemoryEntry, MemoryError>;
    async fn search(
        &self,
        namespace: &str,
        query: &MemoryQuery,
    ) -> Result<Vec<MemoryEntry>, MemoryError>;
    /// None creates; Some updates exactly that version. An expired entry is
    /// absent, so recreation uses None and receives a fresh version identity.
    async fn save(
        &self,
        namespace: &str,
        value: &MemoryWrite,
        expected: Option<&str>,
    ) -> Result<MemoryEntry, MemoryError>;
    async fn delete(&self, namespace: &str, id: &str, expected: &str) -> Result<(), MemoryError>;
}
