//! Persistent user questions and decisions, shared by execution strategies.
use crate::*;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InteractionRecord {
    pub session_id: String,
    /// Stable logical run identity.
    pub run_id: RunId,
    pub request: InteractionRequest,
    /// The original provider call and arguments. Approval always targets this call.
    pub tool_call: Option<ToolCall>,
    pub response: Option<InteractionResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_provenance: Option<MessageProvenance>,
    pub created_at_unix_ms: u64,
    pub answered_at_unix_ms: Option<u64>,
}
impl InteractionRecord {
    pub fn resume_request(&self) -> RunRequest {
        let mut request = RunRequest::new(&self.session_id, "");
        request.message_provenance = self.response_provenance.clone();
        request.resume = Some(ResumeRequest {
            run_id: self.run_id,
            response: self.response.clone(),
        });
        request
    }
}

#[crate::extension_point(single, adapter = crate::InteractionStorePlugin)]
#[async_trait]
pub trait InteractionStore: Send + Sync {
    /// Idempotent publication. Reusing an ID with different ownership or payload must fail.
    async fn publish(&self, record: &InteractionRecord) -> Result<(), StateError>;
    /// Publish the batch atomically in one storage operation.
    async fn publish_many(&self, records: &[InteractionRecord]) -> Result<(), StateError>;
    async fn get(&self, session_id: &str, id: RunId) -> Result<InteractionRecord, StateError>;
    /// Batched lookup in one storage round trip. Records must come back in the
    /// requested order; a missing id is an error, matching `get`. Every store must provide native batch lookup.
    async fn get_many(
        &self,
        session_id: &str,
        ids: &[RunId],
    ) -> Result<Vec<InteractionRecord>, StateError>;
    /// Ordered optional lookup for callers that tolerate deleted records.
    async fn find_many(
        &self,
        session_id: &str,
        ids: &[RunId],
    ) -> Result<Vec<Option<InteractionRecord>>, StateError>;
    /// Permanently close a terminal run's interactions without fabricating answers.
    /// Idempotent and durable: late publications must not reopen pending work,
    /// and new answers must be rejected. Existing records remain auditable.
    async fn close_run(&self, session_id: &str, run_id: RunId) -> Result<(), StateError>;
    /// Pending includes expired requests, but excludes closed runs.
    async fn pending(&self, session_id: &str) -> Result<Vec<InteractionRecord>, StateError>;
    /// Validate and save atomically. Identical answers are idempotent; conflicting ones fail.
    async fn answer(
        &self,
        session_id: &str,
        response: &InteractionResponse,
    ) -> Result<InteractionRecord, AgentError>;
    /// Atomically persist answer attribution when supported by the store.
    /// An unknown sender may be filled once; an already attributed answer is immutable.
    /// The default preserves compatibility for stores without attribution persistence.
    async fn answer_with_provenance(
        &self,
        session_id: &str,
        response: &InteractionResponse,
        provenance: Option<&MessageProvenance>,
    ) -> Result<InteractionRecord, AgentError> {
        let mut record = self.answer(session_id, response).await?;
        if provenance.is_some()
            && record.response_provenance.is_some()
            && provenance != record.response_provenance.as_ref()
        {
            return Err(StateError::Conflict.into());
        }
        if record.response_provenance.is_none() {
            record.response_provenance = provenance.cloned();
        }
        Ok(record)
    }
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
        cursor: &ReactCursor,
        context: &RunContext,
    ) -> Result<(), AgentError> {
        let mut publications = Vec::new();
        let ids = requests
            .iter()
            .map(|request| request.id)
            .collect::<Vec<_>>();
        let previous = self.0.find_many(&context.session_id, &ids).await?;
        for (request, previous) in requests.iter().zip(previous) {
            let call = match cursor {
                ReactCursor::Tools { executions, .. } => executions.iter().find(|e| match request.kind {
                    InteractionKind::Approval { invocation_id } => e.invocation_id == invocation_id,
                    InteractionKind::Input => matches!(&e.state, ToolExecutionState::Finished(output) if matches!(&output.effect, ToolEffect::RequestInput(r) if r.id == request.id)),
                }).map(|e| e.call.clone()),
                ReactCursor::Model => None,
            };
            // A previously published question may already have left the tool cursor.
            if call.is_none() {
                match previous {
                    Some(previous)
                        if previous.run_id == context.run_id && previous.request == *request =>
                    {
                        continue;
                    }
                    Some(_) => return Err(StateError::Conflict.into()),
                    None => {}
                }
            }
            publications.push(InteractionRecord {
                session_id: context.session_id.clone(),
                run_id: context.run_id,
                request: request.clone(),
                tool_call: call,
                response: None,
                response_provenance: None,
                created_at_unix_ms: unix_time_ms(),
                answered_at_unix_ms: None,
            });
        }
        self.0.publish_many(&publications).await?;
        Ok(())
    }
}
