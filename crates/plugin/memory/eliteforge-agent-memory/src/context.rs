//! Scoped memory providers reuse the Framework context layout and assembler.
use crate::{MemoryRevision, MemoryScope};
use async_trait::async_trait;
use eliteforge_agent_core::*;
use std::sync::Arc;

#[derive(Clone)]
pub struct ScopedMemoryContext {
    pub revision: MemoryRevision,
    pub block: ContextBlock,
}
/// This boundary reads prepared in-memory snapshots only. Async retrieval and
/// its cancellation/IO deadline belong in the contributor, never this sync read.
pub trait MemoryContextSource: Send + Sync {
    fn read(&self, scope: &MemoryScope) -> Result<ScopedMemoryContext, MemoryError>;
}
/// A live revision check is separate from the prepared data snapshot. Hosts may
/// perform authorized async retrieval here; the assembler enforces run deadlines.
#[async_trait]
pub trait MemoryContextRevisionGuard: Send + Sync {
    async fn ensure_current(&self, expected: &MemoryRevision) -> Result<(), MemoryError>;
}
#[async_trait]
impl MemoryContextRevisionGuard for ScopedMemoryContext {
    async fn ensure_current(&self, expected: &MemoryRevision) -> Result<(), MemoryError> {
        expected.ensure_current(&self.revision)
    }
}
impl MemoryContextSource for ScopedMemoryContext {
    fn read(&self, scope: &MemoryScope) -> Result<ScopedMemoryContext, MemoryError> {
        if &self.revision.scope != scope {
            return Err(MemoryError::Invalid("memory provider crossed scope".into()));
        }
        Ok(self.clone())
    }
}
/// One independent provider. Instructions and protocol history use builtins.
pub struct MemoryContextContributor {
    id: String,
    scope: MemoryScope,
    source: Arc<dyn MemoryContextSource>,
}
impl MemoryContextContributor {
    pub fn new(
        id: impl Into<String>,
        scope: MemoryScope,
        source: Arc<dyn MemoryContextSource>,
    ) -> Result<Self, AgentError> {
        let id = id.into();
        if id.trim().is_empty() || [INSTRUCTIONS_BLOCK, HISTORY_BLOCK].contains(&id.as_str()) {
            return Err(AgentError::InvalidConfiguration(
                "memory provider requires a data block ID".into(),
            ));
        }
        Ok(Self { id, scope, source })
    }
}
#[async_trait]
impl ContextContributor for MemoryContextContributor {
    fn block_id(&self) -> &str {
        &self.id
    }
    async fn contribute(&self, input: &ContextInput<'_>) -> Result<ContextBlock, AgentError> {
        input.run.check_cancelled()?;
        let context = self.source.read(&self.scope)?;
        if context.revision.scope != self.scope
            || context.block.id != self.id
            || context.block.trust != ContextTrust::Data
        {
            return Err(
                MemoryError::Invalid("memory provider changed scope, ID or trust".into()).into(),
            );
        }
        let mut block = context.block;
        block.source_version = Some(format!(
            "{}:{}",
            context.revision.generation, context.revision.revision
        ));
        Ok(block)
    }
}
/// Fixed blocks keep their actual capped quotas; builtin history borrows the rest.
/// This policy never changes provider order or accepts unbounded protocol costs.
pub struct BorrowUnusedMemoryBudget;
#[async_trait]
impl ContextBudgetPolicy for BorrowUnusedMemoryBudget {
    async fn allocate(
        &self,
        blocks: &[ContextBlockConfig],
        available: ContextBudget,
    ) -> Result<Vec<ContextBudget>, AgentError> {
        IndependentBlockBudget.allocate(blocks, available).await
    }
    async fn allocate_sources(
        &self,
        blocks: &[ContextBlockConfig],
        available: ContextBudget,
        source_tokens: &[usize],
    ) -> Result<Vec<ContextBudget>, AgentError> {
        if blocks.len() != source_tokens.len() {
            return Err(AgentError::InvalidConfiguration(
                "memory source cost count mismatch".into(),
            ));
        }
        let mut remaining = available.max_tokens;
        let mut budgets = Vec::with_capacity(blocks.len());
        for (block, &tokens) in blocks.iter().zip(source_tokens) {
            let mut budget = configured_block_budget(block, available)?;
            if block.id == INSTRUCTIONS_BLOCK && tokens > budget.max_tokens {
                return Err(ContextError::BudgetExceeded(
                    "protected instructions exceed their block budget".into(),
                )
                .into());
            }
            if block.id == HISTORY_BLOCK {
                budget.max_tokens = 0;
            } else {
                budget.max_tokens = budget.max_tokens.min(tokens);
                remaining = remaining.checked_sub(budget.max_tokens).ok_or_else(|| {
                    ContextError::BudgetExceeded(
                        "fixed memory block quotas exceed input capacity".into(),
                    )
                })?;
            }
            budgets.push(budget);
        }
        for (block, budget) in blocks.iter().zip(&mut budgets) {
            if block.id == HISTORY_BLOCK {
                budget.max_tokens = configured_block_budget(block, available)?
                    .max_tokens
                    .min(remaining);
            }
        }
        Ok(budgets)
    }
}
/// A data-only bounding strategy. Host callbacks map typed fields; binary search,
/// actual estimator admission, cancellation and maximum retention are SDK-owned.
type MemoryDataBound = dyn Fn(&Message, usize) -> Message + Send + Sync;
pub struct BoundMemoryData {
    bound: Arc<MemoryDataBound>,
}
impl BoundMemoryData {
    pub fn new(bound: impl Fn(&Message, usize) -> Message + Send + Sync + 'static) -> Self {
        Self {
            bound: Arc::new(bound),
        }
    }
}
#[async_trait]
impl ContextReducer for BoundMemoryData {
    async fn reduce(&self, input: &ContextReductionInput<'_>) -> Result<Vec<Message>, AgentError> {
        input.run.check_cancelled()?;
        let target = input
            .compression
            .map_or(input.budget.max_tokens, |directive| {
                directive.target_tokens.min(input.budget.max_tokens)
            });
        let mut remaining = target;
        let mut output = Vec::new();
        for entry in &input.block.messages {
            input.run.check_cancelled()?;
            if output.len() >= input.budget.max_messages {
                break;
            }
            let size = input.estimator.message(input.model, entry).await?;
            if size <= remaining {
                output.push(entry.clone());
                remaining -= size;
                continue;
            }
            let mut best = (self.bound)(entry, 0);
            if input.estimator.message(input.model, &best).await? > remaining {
                continue;
            }
            let mut low = 0;
            let mut high = remaining;
            while low < high {
                input.run.check_cancelled()?;
                let midpoint = low + (high - low) / 2 + (high - low) % 2;
                let candidate = (self.bound)(entry, midpoint);
                if input.estimator.message(input.model, &candidate).await? <= remaining {
                    low = midpoint;
                    best = candidate;
                } else {
                    high = midpoint - 1;
                }
            }
            remaining =
                remaining.saturating_sub(input.estimator.message(input.model, &best).await?);
            output.push(best);
        }
        Ok(output)
    }
}

/// The semantic data strategy reuses ModelCompressor and its versioned store.
/// It is distinct from entry selection or text bounding (which are lossy trims).
#[async_trait]
pub trait MemoryReductionAuditSink: Send + Sync {
    async fn record(
        &self,
        run: &RunContext,
        block: &ContextBlock,
        result: &crate::MemoryCompactionResult,
    ) -> Result<(), AgentError>;
}
pub struct MemorySemanticDataReducer {
    compactor: crate::MemoryCompactor,
    revision: MemoryRevision,
    source: Option<Arc<dyn MemoryContextRevisionGuard>>,
    audit: Option<Arc<dyn MemoryReductionAuditSink>>,
}
impl MemorySemanticDataReducer {
    pub fn new(
        model: ModelRef,
        options: crate::CompressionOptions,
        revision: MemoryRevision,
    ) -> Result<Self, ContextError> {
        Ok(Self {
            compactor: crate::MemoryCompactor::new(model, options)?,
            revision,
            source: None,
            audit: None,
        })
    }
    pub fn with_store(
        mut self,
        store: Arc<dyn crate::SummaryStore>,
        policy: impl Into<String>,
    ) -> Result<Self, ContextError> {
        self.compactor = self.compactor.with_store(store, policy)?;
        Ok(self)
    }
    pub fn with_audit_sink(mut self, audit: Arc<dyn MemoryReductionAuditSink>) -> Self {
        self.audit = Some(audit);
        self
    }
    pub fn with_source_guard(mut self, source: Arc<dyn MemoryContextRevisionGuard>) -> Self {
        self.source = Some(source);
        self
    }
}
#[async_trait]
impl ContextReducer for MemorySemanticDataReducer {
    async fn reduce(&self, input: &ContextReductionInput<'_>) -> Result<Vec<Message>, AgentError> {
        input.run.check_cancelled()?;
        if input.block.messages.is_empty() {
            return Ok(Vec::new());
        }
        let original_tokens =
            context_tokens(&input.block.messages, input.model, input.estimator).await?;
        if input.compression.is_none()
            && original_tokens <= input.budget.max_tokens
            && input.block.messages.len() <= input.budget.max_messages
        {
            return Ok(input.block.messages.clone());
        }
        if let Some(source) = &self.source {
            source.ensure_current(&self.revision).await?;
        }
        let before = input.run.usage();
        let result = self
            .compactor
            .compact_data_with_usage(input, &self.revision)
            .await;
        if let Some(audit) = &self.audit {
            match &result {
                Ok(result) => audit.record(input.run, input.block, result).await?,
                Err(_) => {
                    let after = input.run.usage();
                    let usage = Usage {
                        input_tokens: after.input_tokens.saturating_sub(before.input_tokens),
                        output_tokens: after.output_tokens.saturating_sub(before.output_tokens),
                    };
                    if usage != Usage::default() {
                        audit
                            .record(
                                input.run,
                                input.block,
                                &crate::MemoryCompactionResult {
                                    summary: String::new(),
                                    usage,
                                    provider_usage: None,
                                    provider_metrics: None,
                                },
                            )
                            .await?;
                    }
                }
            }
        }
        if let Some(source) = &self.source {
            source.ensure_current(&self.revision).await?;
        }
        match result {
            Ok(result) => Ok(vec![Message::user(result.summary)]),
            // A soft target must never remove protected data simply because the
            // minimum summary framing cannot fit. The assembler checks hard cap.
            Err(error)
                if matches!(
                    error.primary(),
                    AgentError::Context(ContextError::BudgetExceeded(_))
                ) && original_tokens <= input.budget.max_tokens
                    && input.block.messages.len() <= input.budget.max_messages =>
            {
                Ok(input.block.messages.clone())
            }
            Err(error) => Err(error),
        }
    }
}
