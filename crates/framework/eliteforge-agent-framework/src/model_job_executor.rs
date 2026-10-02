//! A workflow may narrow calls, never bypass the effective job dispatch gate.
use super::*;
use std::sync::atomic::Ordering;
pub(super) struct WorkflowExecutor<'a> {
    inner: &'a ModelInvoker,
    state: &'a JobState,
    max_calls: usize,
    admission: tokio::sync::Mutex<()>,
}
impl<'a> WorkflowExecutor<'a> {
    pub fn new(inner: &'a ModelInvoker, state: &'a JobState, max_calls: usize) -> Self {
        Self {
            inner,
            state,
            max_calls,
            admission: tokio::sync::Mutex::new(()),
        }
    }
    fn remaining(&self) -> Result<usize, AgentError> {
        let remaining = self
            .max_calls
            .saturating_sub(self.state.calls.load(Ordering::Acquire));
        if remaining == 0 {
            Err(AgentError::Run("model job call budget exhausted".into()))
        } else {
            Ok(remaining)
        }
    }
}
#[async_trait]
impl ChatExecutor for WorkflowExecutor<'_> {
    fn catalog(&self) -> &ModelCatalog {
        self.inner.catalog()
    }
    fn catalog_snapshot(&self) -> Arc<ModelCatalog> {
        self.inner.catalog_snapshot()
    }
    async fn invoke(
        &self,
        call: ModelCall<'_>,
        memory: &mut dyn ConversationSession,
        context: &RunContext,
    ) -> Result<ChatResponse, AgentError> {
        let _admission = self.admission.lock().await;
        context.check_cancelled()?;
        let remaining = self.remaining()?;
        let mut limits = call.limits.clone();
        limits.max_model_calls_per_step = Some(
            limits
                .max_model_calls_per_step
                .map_or(remaining, |limit| limit.min(remaining)),
        );
        self.inner
            .invoke(
                ModelCall {
                    stage: call.stage,
                    step: call.step,
                    instructions: call.instructions,
                    tools: call.tools,
                    limits: &limits,
                    options: call.options,
                },
                memory,
                context,
            )
            .await
    }
    async fn invoke_auxiliary(
        &self,
        model: &ModelRef,
        request: ChatRequest,
        stage: &str,
        limits: &RunLimits,
        context: &RunContext,
    ) -> Result<ChatResponse, AgentError> {
        // Serial admission prevents two callbacks observing the same last slot.
        let _admission = self.admission.lock().await;
        context.check_cancelled()?;
        self.remaining()?;
        self.inner
            .invoke_auxiliary(model, request, stage, limits, context)
            .await
    }
}
