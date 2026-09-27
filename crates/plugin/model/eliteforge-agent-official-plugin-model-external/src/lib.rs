//! Native external-Agent transports. The enclosing Framework run owns the
//! invocation future; cancelling it releases CLI readers and process groups.
//! Hosts supply authorized configuration and product prompt/context projections.
mod bridge;
mod cli;
mod stage;
pub use bridge::JsonAgentBridge;
pub use stage::*;
mod output;
pub use cli::ExternalCli;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalCliKind {
    Codex,
    OpenCode,
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
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalAgentEventKind {
    Text,
    ReasoningSummary,
}
#[derive(Debug, Clone)]
pub struct ExternalAgentEvent {
    pub kind: ExternalAgentEventKind,
    pub delta: String,
}
impl ExternalAgentEvent {
    pub fn text_delta(delta: impl Into<String>) -> Self {
        Self {
            kind: ExternalAgentEventKind::Text,
            delta: delta.into(),
        }
    }
    pub fn reasoning_summary_delta(delta: impl Into<String>) -> Self {
        Self {
            kind: ExternalAgentEventKind::ReasoningSummary,
            delta: delta.into(),
        }
    }
}
pub type ExternalAgentSink = Arc<dyn Fn(ExternalAgentEvent) + Send + Sync>;

#[derive(Debug)]
pub enum TransportError {
    PromptRequired,
    Failed(String),
}
impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PromptRequired => f.write_str("prompt is required"),
            Self::Failed(message) => f.write_str(message),
        }
    }
}
impl std::error::Error for TransportError {}
