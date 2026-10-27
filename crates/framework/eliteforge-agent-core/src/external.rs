//! An external Agent owns its conversation, tools, approvals and checkpoints.
use crate::*;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Opaque identifiers understood by the external runtime and its host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalSessionRef {
    pub session_id: String,
    pub run_id: Option<String>,
}

/// Host-authorized process directory supplied for an external execution.
#[derive(Debug, Clone)]
pub struct ExternalWorkingDirectory(pub String);

pub struct ExternalAgentRequest {
    pub annotations: Vec<MessageAnnotation>,
    pub input: Content,
    pub model: Option<String>,
    pub session: Option<ExternalSessionRef>,
    pub extensions: Extensions,
}

pub struct ExternalAgentResult {
    pub content: Content,
    pub structured: Option<Value>,
    pub session: Option<ExternalSessionRef>,
    pub usage: Usage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ExternalAgentEvent {
    TextDelta(String),
    ReasoningDelta(String),
    Status(String),
    Tool { name: String, detail: Value },
    Approval { detail: Value },
    Session(ExternalSessionRef),
}

#[async_trait]
pub trait ExternalAgentEventSink: Send + Sync {
    async fn emit(&self, event: ExternalAgentEvent);
}

#[crate::extension_point(select, adapter = crate::ExternalAgentPlugin)]
#[async_trait]
pub trait ExternalAgent: Send + Sync {
    async fn run(
        &self,
        request: ExternalAgentRequest,
        sink: &dyn ExternalAgentEventSink,
    ) -> Result<ExternalAgentResult, AgentError>;
}
