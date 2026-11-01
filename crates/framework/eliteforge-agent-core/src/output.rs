//! User-facing output, independent of its transport protocol.
use crate::{Content, RunResult, ToolCall, ToolOutput};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Events for presentation adapters. Provider-private model context is never
/// included. Tool outputs must already have passed output middleware/offloading.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OutputEvent {
    StepStarted {
        id: String,
        name: String,
    },
    StepFinished {
        id: String,
    },
    MessageStarted {
        message_id: String,
    },
    MessageMetadata {
        message_id: String,
        step_id: String,
        output_index: usize,
        phase: Option<String>,
    },
    TextDelta {
        message_id: String,
        delta: String,
    },
    ReasoningDelta {
        message_id: String,
        delta: String,
    },
    MessageFinished {
        message_id: String,
        content: Content,
        structured: Option<Value>,
        tool_calls: Vec<ToolCall>,
    },
    MessageAborted {
        message_id: String,
    },
    ToolCalls {
        message_id: String,
        calls: Vec<ToolCall>,
    },
    ToolResult {
        /// Retain the originating call's invocation_id, including after resume.
        call: ToolCall,
        output: ToolOutput,
    },
    RunResult(Box<RunResult>),
    /// Host-defined state intended for the client, never an execution checkpoint.
    StateSnapshot(Value),
    /// RFC 6902 patch document; the transport adapter validates its shape.
    StateDelta(Value),
    ActivitySnapshot {
        id: String,
        activity_type: String,
        content: serde_json::Map<String, Value>,
    },
    ActivityDelta {
        id: String,
        activity_type: String,
        patch: Value,
    },
    Custom {
        name: String,
        value: Value,
    },
}
