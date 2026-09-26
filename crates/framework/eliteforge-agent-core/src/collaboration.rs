//! Contracts used by official agent and thread collaboration tools.
//!
//! The core only defines the boundary. A framework or upper application owns
//! the actual agent registry, authorization and durable task lifecycle.
use crate::{
    AgentError, Content, ConversationSnapshot, InteractionRequest, RunContext, RunId, RunRequest,
    RunResult, RunStatus,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// A runtime-owned conversation that can be addressed by collaboration tools.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ThreadStatus {
    Running,
    Completed,
    AwaitingInput,
    StepLimit,
    ToolCallLimit,
    Failed,
    Cancelled,
}

/// Why an independent thread started another run.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThreadRunCause {
    Created,
    Message,
    InteractionResponse,
}

/// Observable collaboration lifecycle published on the run that initiated it.
///
/// Task text, thread messages and interaction answers are deliberately absent.
/// The ordinary tool result remains the authoritative result payload, while
/// these events let transports present child-agent and thread lifecycle state.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CollaborationEvent {
    ChildRunStarted {
        child_run_id: RunId,
        session_id: String,
        depth: usize,
    },
    ChildRunFinished {
        child_run_id: RunId,
        status: RunStatus,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        interactions: Vec<InteractionRequest>,
    },
    ChildRunFailed {
        child_run_id: RunId,
        cancelled: bool,
        message: String,
    },
    ThreadCreated {
        summary: ThreadSummary,
    },
    ThreadRunStarted {
        thread_id: String,
        run_id: RunId,
        cause: ThreadRunCause,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interaction_id: Option<RunId>,
    },
    ThreadRunFinished {
        thread_id: String,
        run_id: RunId,
        status: ThreadStatus,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        interactions: Vec<InteractionRequest>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    ThreadClosed {
        thread_id: String,
    },
}

impl From<&RunStatus> for ThreadStatus {
    fn from(status: &RunStatus) -> Self {
        match status {
            RunStatus::Completed => Self::Completed,
            RunStatus::AwaitingInput => Self::AwaitingInput,
            RunStatus::StepLimit => Self::StepLimit,
            RunStatus::ToolCallLimit => Self::ToolCallLimit,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub status: ThreadStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_run_id: Option<RunId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadResult {
    pub run_id: RunId,
    pub output: String,
    pub content: Content,
    pub structured: Option<serde_json::Value>,
    pub interactions: Vec<InteractionRequest>,
    pub status: RunStatus,
    pub usage: crate::Usage,
    pub steps: usize,
    pub tool_calls: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildRunResult {
    pub run_id: RunId,
    pub result: RunResult,
}

impl ThreadResult {
    pub fn from_run(run_id: RunId, result: &RunResult) -> Self {
        Self {
            run_id,
            output: result.output.clone(),
            content: result.content.clone(),
            structured: result.structured.clone(),
            interactions: result.interactions.clone(),
            status: result.status.clone(),
            usage: result.usage.clone(),
            steps: result.steps,
            tool_calls: result.tool_calls,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadView {
    pub summary: ThreadSummary,
    pub conversation: ConversationSnapshot,
}

/// Bridge used by the official collaboration tools.
///
/// `spawn_child` is an in-tree delegation: it inherits the parent run's
/// cancellation, deadline, lineage and reserved allowance. Thread operations
/// intentionally do not inherit those properties; they address independent
/// sessions and are expected to be governed by the implementation's quota and
/// authorization rules.
#[async_trait]
pub trait AgentCollaborationRuntime: Send + Sync {
    async fn spawn_child(
        &self,
        request: RunRequest,
        parent: &RunContext,
    ) -> Result<ChildRunResult, AgentError>;

    async fn create_thread(
        &self,
        request: RunRequest,
        title: Option<String>,
        parent: &RunContext,
    ) -> Result<ThreadSummary, AgentError>;

    async fn send_thread(
        &self,
        thread_id: &str,
        request: RunRequest,
        parent: &RunContext,
    ) -> Result<ThreadSummary, AgentError>;

    async fn answer_thread(
        &self,
        thread_id: &str,
        interaction_id: RunId,
        value: serde_json::Value,
        parent: &RunContext,
    ) -> Result<ThreadSummary, AgentError>;

    async fn wait_thread(
        &self,
        thread_id: &str,
        run_id: Option<RunId>,
        parent: &RunContext,
    ) -> Result<ThreadResult, AgentError>;

    async fn read_thread(&self, thread_id: &str, limit: usize) -> Result<ThreadView, AgentError>;

    async fn list_threads(&self, parent: &RunContext) -> Result<Vec<ThreadSummary>, AgentError>;

    async fn close_thread(&self, thread_id: &str, parent: &RunContext) -> Result<(), AgentError>;
}

/// Run-local extension used by official tools to find the configured bridge.
#[derive(Clone)]
pub struct AgentCollaborationService(pub std::sync::Arc<dyn AgentCollaborationRuntime>);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_status_preserves_every_run_terminal_state() {
        for (run, thread) in [
            (RunStatus::Completed, ThreadStatus::Completed),
            (RunStatus::AwaitingInput, ThreadStatus::AwaitingInput),
            (RunStatus::StepLimit, ThreadStatus::StepLimit),
            (RunStatus::ToolCallLimit, ThreadStatus::ToolCallLimit),
        ] {
            assert_eq!(ThreadStatus::from(&run), thread);
        }
    }
}
