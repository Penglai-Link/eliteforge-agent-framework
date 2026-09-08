//! Durable ReAct and configurable stage execution over the same model/tool invokers.
use crate::*;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashSet},
    time::Duration,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowStage {
    pub name: String,
    pub instructions: Vec<Message>,
    pub generation: Option<GenerationOptions>,
    /// None permits the run's exposed tool catalog; Some is an exact subset.
    pub tools: Option<BTreeSet<String>>,
}
impl WorkflowStage {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            instructions: Vec::new(),
            generation: None,
            tools: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowDefinition {
    pub id: String,
    pub version: u32,
    pub stages: Vec<WorkflowStage>,
}
impl WorkflowDefinition {
    pub fn validate(&self) -> Result<(), AgentError> {
        if self.id.trim().is_empty() || self.version == 0 || self.stages.is_empty() {
            return Err(AgentError::InvalidConfiguration(
                "workflow requires identity, version and stages".into(),
            ));
        }
        let mut names = HashSet::new();
        for stage in &self.stages {
            if stage.name.trim().is_empty() || !names.insert(&stage.name) {
                return Err(AgentError::InvalidConfiguration(
                    "workflow stage names must be nonempty and unique".into(),
                ));
            }
            if stage.instructions.iter().any(|m| {
                !matches!(m.role, Role::System | Role::Developer)
                    || !m.tool_calls.is_empty()
                    || m.tool_call_id.is_some()
                    || m.tool_result.is_some()
            }) {
                return Err(AgentError::InvalidConfiguration(
                    "stage instructions must use trusted instruction roles".into(),
                ));
            }
            if let Some(options) = &stage.generation {
                options.validate()?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WorkflowCursor {
    Model,
    Tools {
        assistant: Box<Message>,
        executions: Vec<ToolExecution>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowState {
    pub definition: WorkflowDefinition,
    pub request: RunRequest,
    #[serde(default)]
    pub conversation_generation: Option<String>,
    pub stage: usize,
    pub steps: usize,
    pub tool_calls: usize,
    pub cursor: WorkflowCursor,
    pub visible_tools: BTreeSet<String>,
    #[serde(default)]
    pub revoked_tools: BTreeSet<String>,
    pub interactions: Vec<InteractionRequest>,
    pub answers: Vec<InteractionResponse>,
    pub output: Content,
    pub structured: Option<serde_json::Value>,
    pub usage: Usage,
    pub budget_usage: Usage,
    #[serde(default)]
    pub child_runs: usize,
    pub outcome: Option<RunStatus>,
}

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
