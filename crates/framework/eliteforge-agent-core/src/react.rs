//! Durable ReAct model and tool execution state.
use crate::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ReactCursor {
    Model,
    Tools {
        assistant: Box<Message>,
        executions: Vec<ToolExecution>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactState {
    pub request: RunRequest,
    #[serde(default)]
    pub conversation_generation: Option<String>,
    pub steps: usize,
    #[serde(default)]
    pub phase_continuations: usize,
    pub tool_calls: usize,
    #[serde(default)]
    pub tool_batch_limit_reached: bool,
    pub cursor: ReactCursor,
    /// Complete provider response committed with public history before the loop
    /// advances. Recovery consumes it without dispatching the model again.
    #[serde(default)]
    pub pending_model_response: Option<ChatResponse>,
    /// Original response order awaiting completion of the tool batch.
    #[serde(default)]
    pub pending_assistant_messages: Vec<Message>,
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
