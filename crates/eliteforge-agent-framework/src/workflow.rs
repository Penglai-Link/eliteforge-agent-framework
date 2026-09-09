use crate::*;
use async_trait::async_trait;
use std::time::Duration;
pub struct WorkflowLoop(crate::model_execution::ModelExecutionLoop);
impl WorkflowLoop {
    pub fn new(definition: WorkflowDefinition) -> Result<Self, AgentError> {
        Ok(Self(crate::model_execution::ModelExecutionLoop::new(
            definition,
        )?))
    }
    pub fn with_lease_duration(self, duration: Duration) -> Result<Self, AgentError> {
        Ok(Self(self.0.with_lease_duration(duration)?))
    }
}
#[async_trait]
impl AgentLoop for WorkflowLoop {
    fn supports_resume(&self) -> bool {
        true
    }
    fn requires_interactions(&self) -> bool {
        true
    }
    fn supports_tool_policy(&self) -> bool {
        true
    }
    fn requires_model(&self) -> bool {
        true
    }
    fn requires_state(&self) -> bool {
        true
    }
    fn cooperative_cancellation(&self) -> bool {
        true
    }
    async fn run(
        &self,
        request: &RunRequest,
        context: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        self.0.run(request, context, services).await
    }
}
