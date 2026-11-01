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

/// Trusted host capability: this run may suspend for an answer from its caller.
/// Tool arguments cannot grant this capability to a top-level run.
#[derive(Clone, Copy, Debug)]
pub struct CallerInteractionScope;

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

/// Lifecycle of one addressable child run, including a paused synchronous child.
///
/// A detached child is an in-tree delegation that outlives the parent run:
/// it keeps the parent's lineage and budget reservation, but owns its own
/// cancellation token and deadline. Unlike a thread it is an in-memory,
/// single-run lifecycle governed by the owning host registry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ChildRunStatus {
    Running,
    Completed,
    AwaitingInput,
    StepLimit,
    ToolCallLimit,
    Failed,
    Cancelled,
}

impl From<&RunStatus> for ChildRunStatus {
    fn from(status: &RunStatus) -> Self {
        match status {
            RunStatus::Completed => Self::Completed,
            RunStatus::AwaitingInput => Self::AwaitingInput,
            RunStatus::StepLimit => Self::StepLimit,
            RunStatus::ToolCallLimit => Self::ToolCallLimit,
        }
    }
}

/// Addressable snapshot of one detached child run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetachedChildSummary {
    /// Identity used by every child operation.
    pub child_run_id: RunId,
    /// The native session backing the child run.
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_code: Option<String>,
    pub status: ChildRunStatus,
}

/// Status plus recent conversation of one detached child run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildRunView {
    pub summary: DetachedChildSummary,
    pub conversation: ConversationSnapshot,
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
    ChildRunDetached {
        child_run_id: RunId,
        session_id: String,
        depth: usize,
    },
    ThreadCreated {
        summary: ThreadSummary,
    },
    ThreadRunStarted {
        session_id: String,
        run_id: RunId,
        cause: ThreadRunCause,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interaction_id: Option<RunId>,
    },
    ThreadRunFinished {
        session_id: String,
        run_id: RunId,
        status: ThreadStatus,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        interactions: Vec<InteractionRequest>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    ThreadClosed {
        session_id: String,
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
    /// The native Session ID. Thread operations never allocate a second identity.
    pub session_id: String,
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
/// For `spawn_child` and `create_thread`, `RunRequest::agent_code` carries the
/// optional application-owned target. Implementations must resolve and authorize
/// it using the caller context, then clear it before executing on the target
/// Agent. Never silently fall back for unknown or unauthorized codes. Independent
/// threads must retain their target binding for all subsequent operations.
///
/// `spawn_child` is an in-tree delegation: it inherits the parent run's
/// cancellation, deadline, lineage and reserved allowance. `spawn_child_detached`
/// keeps lineage, depth and budget reservation but owns an independent
/// cancellation token and deadline, so the child keeps running after the parent
/// run ends; the caller owns the returned handle and governs its lifecycle.
/// Thread operations intentionally do not inherit those properties; they
/// address independent, durable sessions and are expected to be governed by
/// the implementation's quota and authorization rules.
#[async_trait]
pub trait AgentCollaborationRuntime: Send + Sync {
    /// Read one currently authorized target's configuration without starting it.
    async fn agent_details(
        &self,
        _agent_code: &str,
        _parent: &RunContext,
    ) -> Result<serde_json::Value, AgentError> {
        Err(AgentError::InvalidConfiguration(
            "Agent details lookup is unavailable".into(),
        ))
    }

    async fn spawn_child(
        &self,
        request: RunRequest,
        parent: &RunContext,
    ) -> Result<ChildRunResult, AgentError>;

    /// Dispatch one detached child run and return immediately. The child is
    /// addressable by `child_run_id` through the other child operations.
    async fn spawn_child_detached(
        &self,
        _request: RunRequest,
        _parent: &RunContext,
    ) -> Result<DetachedChildSummary, AgentError> {
        Err(AgentError::InvalidConfiguration(
            "detached child runs are unavailable".into(),
        ))
    }

    /// Wait for one detached child run and return its final result.
    async fn wait_child(
        &self,
        _child_run_id: RunId,
        _parent: &RunContext,
    ) -> Result<ChildRunResult, AgentError> {
        Err(AgentError::InvalidConfiguration(
            "detached child runs are unavailable".into(),
        ))
    }

    /// Queue one steering message for a running detached child.
    async fn send_child(
        &self,
        _child_run_id: RunId,
        _message: String,
        _parent: &RunContext,
    ) -> Result<DetachedChildSummary, AgentError> {
        Err(AgentError::InvalidConfiguration(
            "detached child runs are unavailable".into(),
        ))
    }

    /// Answer one pending interaction and resume the detached child run.
    async fn answer_child(
        &self,
        _child_run_id: RunId,
        _interaction_id: RunId,
        _value: serde_json::Value,
        _parent: &RunContext,
    ) -> Result<DetachedChildSummary, AgentError> {
        Err(AgentError::InvalidConfiguration(
            "detached child runs are unavailable".into(),
        ))
    }

    /// Request cancellation of one detached child run.
    async fn cancel_child(
        &self,
        _child_run_id: RunId,
        _parent: &RunContext,
    ) -> Result<DetachedChildSummary, AgentError> {
        Err(AgentError::InvalidConfiguration(
            "detached child runs are unavailable".into(),
        ))
    }

    /// Read the summary and recent conversation of one detached child run.
    async fn read_child(
        &self,
        _child_run_id: RunId,
        _limit: usize,
    ) -> Result<ChildRunView, AgentError> {
        Err(AgentError::InvalidConfiguration(
            "detached child runs are unavailable".into(),
        ))
    }

    /// List detached child runs visible to the caller.
    async fn list_children(
        &self,
        _parent: &RunContext,
    ) -> Result<Vec<DetachedChildSummary>, AgentError> {
        Err(AgentError::InvalidConfiguration(
            "detached child runs are unavailable".into(),
        ))
    }

    async fn create_thread(
        &self,
        request: RunRequest,
        title: Option<String>,
        parent: &RunContext,
    ) -> Result<ThreadSummary, AgentError>;

    async fn send_thread(
        &self,
        session_id: &str,
        request: RunRequest,
        parent: &RunContext,
    ) -> Result<ThreadSummary, AgentError>;

    async fn answer_thread(
        &self,
        session_id: &str,
        interaction_id: RunId,
        value: serde_json::Value,
        parent: &RunContext,
    ) -> Result<ThreadSummary, AgentError>;

    async fn wait_thread(
        &self,
        session_id: &str,
        run_id: Option<RunId>,
        parent: &RunContext,
    ) -> Result<ThreadResult, AgentError>;

    async fn read_thread(&self, session_id: &str, limit: usize) -> Result<ThreadView, AgentError>;

    async fn list_threads(&self, parent: &RunContext) -> Result<Vec<ThreadSummary>, AgentError>;

    async fn close_thread(&self, session_id: &str, parent: &RunContext) -> Result<(), AgentError>;
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
