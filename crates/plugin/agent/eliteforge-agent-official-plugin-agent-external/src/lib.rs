//! External Agent transports. A Framework run owns the invocation future;
//! dropping it stops local waiting and terminates CLI process groups.
//! The external runtime owns its session, tools, approvals, and checkpoints.
mod bridge;
mod cli;
pub use bridge::JsonAgentBridge;
mod adapter;
pub use adapter::*;
mod output;
mod dsh;
pub use dsh::{DshCliOptions, DshModelProbe, DshModelRecord};
pub use cli::ExternalCli;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalCliKind {
    Codex,
    OpenCode,
    Dsh,
}
#[derive(Debug, Clone)]
pub struct ExternalCliRequest {
    pub prompt: String,
    pub model: Option<String>,
    pub session_id: Option<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalAgentUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub total_tokens: u32,
    pub cached_input_tokens: Option<u32>,
    pub reasoning_tokens: Option<u32>,
}
#[derive(Debug, Clone)]
pub struct ExternalAgentResult {
    pub output: String,
    pub usage: Option<ExternalAgentUsage>,
    pub session_id: Option<String>,
    pub run_id: Option<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalAgentEventKind {
    Text,
    ReasoningSummary,
    ToolObserved,
    ApprovalObserved,
    Status,
    SessionObserved,
}
#[derive(Debug, Clone)]
pub struct ExternalAgentEvent {
    pub kind: ExternalAgentEventKind,
    pub delta: String,
    /// Selected, non-authoritative transport metadata. Never drives Framework tools.
    pub detail: Option<serde_json::Value>,
}
impl ExternalAgentEvent {
    pub fn text_delta(delta: impl Into<String>) -> Self {
        Self {
            kind: ExternalAgentEventKind::Text,
            delta: delta.into(),
            detail: None,
        }
    }
    pub fn reasoning_summary_delta(delta: impl Into<String>) -> Self {
        Self {
            kind: ExternalAgentEventKind::ReasoningSummary,
            delta: delta.into(),
            detail: None,
        }
    }
    pub fn tool_observed(name: impl Into<String>, detail: serde_json::Value) -> Self {
        Self {
            kind: ExternalAgentEventKind::ToolObserved,
            delta: name.into(),
            detail: Some(detail),
        }
    }
    pub fn approval_observed(detail: serde_json::Value) -> Self {
        Self {
            kind: ExternalAgentEventKind::ApprovalObserved,
            delta: "approval".into(),
            detail: Some(detail),
        }
    }
    pub fn status(status: impl Into<String>) -> Self {
        Self {
            kind: ExternalAgentEventKind::Status,
            delta: status.into(),
            detail: None,
        }
    }
    pub fn session_observed(session_id: impl Into<String>) -> Self {
        Self {
            kind: ExternalAgentEventKind::SessionObserved,
            delta: session_id.into(),
            detail: None,
        }
    }
}
pub type ExternalAgentSink = Arc<dyn Fn(ExternalAgentEvent) + Send + Sync>;

#[derive(Debug)]
pub enum TransportError {
    PromptRequired,
    /// Validation or process spawn failed before the CLI could execute.
    NotStarted(String),
    Failed(String),
}
impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PromptRequired => f.write_str("prompt is required"),
            Self::NotStarted(message) | Self::Failed(message) => f.write_str(message),
        }
    }
}
impl std::error::Error for TransportError {}
