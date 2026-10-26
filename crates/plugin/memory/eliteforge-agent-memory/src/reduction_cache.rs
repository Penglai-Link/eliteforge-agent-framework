//! Persist reusable data projections without freezing a contributor's live source.
use crate::{ConversationSummary, SummaryKey, SummaryStore};
use eliteforge_agent_core::*;
use sha2::{Digest, Sha256};
use std::sync::Arc;

pub struct MemoryContextReductionCache {
    store: Arc<dyn SummaryStore>,
    configuration: String,
}
impl MemoryContextReductionCache {
    pub fn new(store: Arc<dyn SummaryStore>, configuration: String) -> Self {
        Self {
            store,
            configuration,
        }
    }
}
fn identity(
    input: &ContextReductionCacheInput<'_>,
    configuration: &str,
) -> Result<(SummaryKey, String), AgentError> {
    let source = serde_json::to_vec(&(&input.block.id, &input.block.messages))
        .map_err(|e| ContextError::Other(e.to_string()))?;
    let digest = format!("{:x}", Sha256::digest(source));
    let budget = input.config.budget;
    let policy = input
        .config
        .compression
        .map(|p| (p.trigger_ratio_bps, p.target_ratio_bps, p.min_gain_tokens));
    let config = serde_json::to_vec(&(
        configuration,
        input.model,
        budget.max_tokens,
        budget.max_ratio_bps,
        budget.max_messages,
        policy,
        &digest,
    ))
    .map_err(|e| ContextError::Other(e.to_string()))?;
    Ok((
        SummaryKey {
            conversation: input.history.key.clone(),
            generation: input.history.generation.clone(),
            block_id: input.block.id.clone(),
            policy_version: "context.data-projection.v1".into(),
            configuration_digest: format!("{:x}", Sha256::digest(config)),
        },
        digest,
    ))
}
#[async_trait::async_trait]
impl ContextReductionCache for MemoryContextReductionCache {
    async fn load(
        &self,
        input: &ContextReductionCacheInput<'_>,
    ) -> Result<Option<Vec<Message>>, AgentError> {
        let (key, digest) = identity(input, &self.configuration)?;
        self.store
            .load(&key)
            .await?
            .filter(|value| value.value.source_digest == digest)
            .map(|value| {
                serde_json::from_str(&value.value.summary)
                    .map_err(|e| ContextError::Other(e.to_string()).into())
            })
            .transpose()
    }
    async fn save(
        &self,
        input: &ContextReductionCacheInput<'_>,
        messages: &[Message],
    ) -> Result<(), AgentError> {
        let (key, digest) = identity(input, &self.configuration)?;
        let prior = self.store.load(&key).await?;
        let value = ConversationSummary {
            format_version: 1,
            source_revision: input.history.revision,
            covered_messages: input.block.messages.len() as u64,
            source_digest: digest,
            summary: serde_json::to_string(messages)
                .map_err(|e| ContextError::Other(e.to_string()))?,
        };
        // A different source has a different key; a competing identical source
        // may retain its valid projection rather than being overwritten.
        let _ = self
            .store
            .compare_exchange(&key, prior.as_ref().map(|p| p.version.as_str()), &value)
            .await?;
        Ok(())
    }
}
