//! Per-step call accounting shared by ordinary attempts and auxiliary reduction.
use crate::*;
use std::sync::atomic::{AtomicUsize, Ordering};
pub(crate) struct StepModels<'a> {
    pub inner: &'a dyn ChatExecutor,
    pub limit: Option<usize>,
    pub used: AtomicUsize,
}
impl StepModels<'_> {
    pub fn reserve(&self, auxiliary: bool) -> Result<(), AgentError> {
        let ceiling = self
            .limit
            .unwrap_or(usize::MAX)
            .saturating_sub(usize::from(auxiliary));
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                (used < ceiling).then(|| used + 1)
            })
            .map(|_| ())
            .map_err(|_| AgentError::Run("model call budget for this step exhausted".into()))
    }
}
#[async_trait::async_trait]
impl ChatExecutor for StepModels<'_> {
    fn catalog(&self) -> &ModelCatalog {
        self.inner.catalog()
    }
    async fn invoke(
        &self,
        _: ModelCall<'_>,
        _: &mut dyn ConversationSession,
        _: &RunContext,
    ) -> Result<ChatResponse, AgentError> {
        Err(AgentError::InvalidConfiguration(
            "context preparation must use auxiliary model calls".into(),
        ))
    }
    async fn invoke_auxiliary(
        &self,
        model: &ModelRef,
        request: ChatRequest,
        stage: &str,
        limits: &RunLimits,
        context: &RunContext,
    ) -> Result<ChatResponse, AgentError> {
        // Keep one dispatch available for the answer after reduction.
        self.reserve(true)?;
        self.inner
            .invoke_auxiliary(model, request, stage, limits, context)
            .await
    }
}
