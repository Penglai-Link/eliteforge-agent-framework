//! Iterative context reduction through controlled auxiliary model invocations.
//! Hosts provide projections and persistence; the framework owns bounded
//! preparation, cancellation and model execution. No reducer invokes a provider.
use crate::*;
use async_trait::async_trait;
use std::{collections::HashSet, sync::Arc};

// Keep the public enum's value shape stable; callers already control the job lifetime.
#[allow(clippy::large_enum_variant)]
pub enum ContextPreparation {
    Ready(ContextWindow),
    Reduce(ContextReductionJob),
}
pub struct ContextReductionJob {
    /// Stable identity of the selected source revision. Repeating it means no progress.
    pub source_revision: String,
    pub model: ModelRef,
    pub request: ChatRequest,
    pub stage: String,
    pub limits: RunLimits,
    pub completion: Arc<dyn ContextReductionCompletion>,
}
#[async_trait]
pub trait ContextReductionCompletion: Send + Sync {
    /// Persist the summary or the failed attempt. Failure must remain visible.
    async fn complete(&self, result: Result<ChatResponse, AgentError>) -> Result<(), AgentError>;
}
#[async_trait]
pub trait ContextPreparationPolicy: Send + Sync {
    /// Return one projection or one reduction plan. This method must not run models.
    async fn prepare(
        &self,
        input: &ContextAssemblyInput<'_>,
        reductions: usize,
    ) -> Result<ContextPreparation, AgentError>;
}
pub struct PreparingContextAssembler {
    policy: Arc<dyn ContextPreparationPolicy>,
    max_reductions: usize,
}
impl PreparingContextAssembler {
    pub fn new(policy: impl ContextPreparationPolicy + 'static, max_reductions: usize) -> Self {
        Self {
            policy: Arc::new(policy),
            max_reductions,
        }
    }
}
#[async_trait]
impl ContextAssembler for PreparingContextAssembler {
    async fn assemble(
        &self,
        input: &ContextAssemblyInput<'_>,
    ) -> Result<ContextWindow, AgentError> {
        let mut revisions = HashSet::new();
        loop {
            input.run.check_cancelled()?;
            match self.policy.prepare(input, revisions.len()).await? {
                ContextPreparation::Ready(window) => return Ok(window),
                ContextPreparation::Reduce(job) => {
                    if revisions.len() >= self.max_reductions.min(input.limits.max_steps) {
                        return Err(AgentError::Run("context reduction limit exhausted".into()));
                    }
                    if job.source_revision.is_empty() || !revisions.insert(job.source_revision) {
                        return Err(AgentError::Run(
                            "context reduction made no source progress".into(),
                        ));
                    }
                    let result = input
                        .models
                        .invoke_auxiliary(
                            &job.model,
                            job.request,
                            &job.stage,
                            &job.limits,
                            input.run,
                        )
                        .await;
                    let failure = result.as_ref().err().map(ToString::to_string);
                    job.completion.complete(result).await?;
                    if let Some(failure) = failure {
                        return Err(AgentError::Run(failure));
                    }
                }
            }
        }
    }
}
