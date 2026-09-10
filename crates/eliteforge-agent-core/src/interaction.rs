//! Persistent user questions and decisions, shared by execution strategies.
use crate::*;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InteractionRecord {
    pub session_id: String,
    /// Stable execution identity, including the parent run for a DAG node.
    pub run_id: RunId,
    pub node_invocation_id: Option<RunId>,
    pub request: InteractionRequest,
    /// The original provider call and arguments. Approval always targets this call.
    pub tool_call: Option<ToolCall>,
    pub response: Option<InteractionResponse>,
    pub created_at_unix_ms: u64,
    pub answered_at_unix_ms: Option<u64>,
}
impl InteractionRecord {
    pub fn resume_request(&self) -> RunRequest {
        let mut request = RunRequest::new(&self.session_id, "");
        request.resume = Some(ResumeRequest {
            run_id: self.run_id,
            response: self.response.clone(),
            resolutions: vec![],
        });
        request
    }
}

#[crate::extension_point(single, adapter = crate::InteractionStorePlugin)]
#[async_trait]
pub trait InteractionStore: Send + Sync {
    /// Idempotent publication. Reusing an ID with different ownership or payload must fail.
    async fn publish(&self, record: &InteractionRecord) -> Result<(), StateError>;
    async fn get(&self, session_id: &str, id: RunId) -> Result<InteractionRecord, StateError>;
    /// Pending includes expired requests; the host can display their expiry.
    async fn pending(&self, session_id: &str) -> Result<Vec<InteractionRecord>, StateError>;
    /// Validate and save atomically. Identical answers are idempotent; conflicting ones fail.
    async fn answer(
        &self,
        session_id: &str,
        response: &InteractionResponse,
    ) -> Result<InteractionRecord, AgentError>;
}

pub struct InteractionStoreService(pub Arc<dyn InteractionStore>);
pub struct InteractionStorePlugin(pub Arc<dyn InteractionStore>);
impl InteractionStorePlugin {
    pub fn new(store: impl InteractionStore + 'static) -> Self {
        Self(Arc::new(store))
    }

    pub fn from_arc(store: Arc<dyn InteractionStore>) -> Self {
        Self(store)
    }
}
#[async_trait]
impl Plugin for InteractionStorePlugin {
    fn id(&self) -> &str {
        "framework.interactions"
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        context.provide(InteractionStoreService(self.0.clone()))?;
        context.contribute_extension(self.0.clone())
    }
}

#[derive(Clone)]
pub struct InteractionManager(pub Arc<dyn InteractionStore>);
impl InteractionManager {
    pub async fn pending(&self, session: &str) -> Result<Vec<InteractionRecord>, AgentError> {
        Ok(self.0.pending(session).await?)
    }
    pub async fn answer(
        &self,
        session: &str,
        id: RunId,
        value: Value,
    ) -> Result<RunRequest, AgentError> {
        Ok(self
            .0
            .answer(
                session,
                &InteractionResponse {
                    interaction_id: id,
                    value,
                },
            )
            .await?
            .resume_request())
    }
    #[doc(hidden)]
    pub async fn publish(
        &self,
        requests: &[InteractionRequest],
        cursor: &WorkflowCursor,
        context: &RunContext,
    ) -> Result<(), AgentError> {
        for request in requests {
            let call = match cursor {
                WorkflowCursor::Tools { executions, .. } => executions.iter().find(|e| match request.kind {
                    InteractionKind::Approval { invocation_id } => e.invocation_id == invocation_id,
                    InteractionKind::Input => matches!(&e.state, ToolExecutionState::Finished(output) if matches!(&output.effect, ToolEffect::RequestInput(r) if r.id == request.id)),
                }).map(|e| e.call.clone()),
                WorkflowCursor::Model => None,
            };
            // A previously published question may already have left the tool cursor.
            if call.is_none() {
                match self.0.get(&context.session_id, request.id).await {
                    Ok(previous)
                        if previous.run_id == context.run_id
                            && previous.request == *request
                            && previous.node_invocation_id
                                == context.node_execution().map(|n| n.invocation_id) =>
                    {
                        continue;
                    }
                    Ok(_) => return Err(StateError::Conflict.into()),
                    Err(StateError::NotFound) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            self.0
                .publish(&InteractionRecord {
                    session_id: context.session_id.clone(),
                    run_id: context.run_id,
                    node_invocation_id: context.node_execution().map(|n| n.invocation_id),
                    request: request.clone(),
                    tool_call: call,
                    response: None,
                    created_at_unix_ms: unix_time_ms(),
                    answered_at_unix_ms: None,
                })
                .await?;
        }
        Ok(())
    }
}
