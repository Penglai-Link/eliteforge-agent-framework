use async_trait::async_trait;
use eliteforge_agent_core::{
    ContextError, ConversationKey, Plugin, PluginContext, PluginError, extension_point,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SummaryKey {
    pub conversation: ConversationKey,
    pub generation: String,
    pub block_id: String,
    pub policy_version: String,
    /// Fingerprint of the algorithm, models, quotas and compression options.
    pub configuration_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationSummary {
    pub format_version: u32,
    /// The snapshot version from which this summary was produced.
    pub source_revision: u64,
    /// Covers sequences 1 through this value, including any preserved instructions.
    /// The digest includes every original field in that prefix.
    pub covered_messages: u64,
    pub source_digest: String,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VersionedSummary {
    /// Opaque CAS identity. Never reused, including after deletion.
    pub version: String,
    pub value: ConversationSummary,
}

#[extension_point(single, adapter = crate::SummaryStorePlugin)]
#[async_trait]
pub trait SummaryStore: Send + Sync {
    async fn load(&self, key: &SummaryKey) -> Result<Option<VersionedSummary>, ContextError>;
    /// None means create only if absent. False means a concurrent writer won;
    /// the caller may still use its valid local projection without overwriting it.
    async fn compare_exchange(
        &self,
        key: &SummaryKey,
        expected: Option<&str>,
        value: &ConversationSummary,
    ) -> Result<bool, ContextError>;
    async fn delete(&self, key: &SummaryKey, expected: &str) -> Result<bool, ContextError>;
}

/// The unique summary-store service belongs with the storage contract, not a
/// particular database implementation.
pub struct SummaryStoreService(pub Arc<dyn SummaryStore>);

pub struct SummaryStorePlugin(pub Arc<dyn SummaryStore>);

impl SummaryStorePlugin {
    pub fn new(store: impl SummaryStore + 'static) -> Self {
        Self(Arc::new(store))
    }

    pub fn from_arc(store: Arc<dyn SummaryStore>) -> Self {
        Self(store)
    }
}

#[async_trait]
impl Plugin for SummaryStorePlugin {
    fn id(&self) -> &str {
        "context.summary-store"
    }

    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        context.provide(SummaryStoreService(self.0.clone()))?;
        context.contribute_extension(self.0.clone())
    }
}
