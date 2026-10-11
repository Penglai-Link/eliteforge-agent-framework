//! Public conversation history. Never contains provider-private model context.
//! Writers commit these records with execution checkpoints, not via event sinks.
use crate::*;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HistoryStatus {
    Running,
    Waiting,
    Completed,
    Interrupted,
    Failed,
    Unknown,
    Planned,
    Approved,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all_fields = "camelCase")]
pub enum HistoryContentBlock {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
    },
    Json {
        value: Value,
    },
    Media {
        mime_type: String,
        source: MediaSource,
        name: Option<String>,
    },
    Resource {
        uri: String,
        mime_type: Option<String>,
        name: Option<String>,
        size_bytes: Option<u64>,
    },
}
pub fn history_content(content: &Content) -> Vec<HistoryContentBlock> {
    content
        .0
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => HistoryContentBlock::Text { text: text.clone() },
            ContentBlock::Json { value } => HistoryContentBlock::Json {
                value: value.clone(),
            },
            ContentBlock::Media { media } => HistoryContentBlock::Media {
                mime_type: media.mime_type.clone(),
                source: media.source.clone(),
                name: media.name.clone(),
            },
            ContentBlock::Resource { resource } => HistoryContentBlock::Resource {
                uri: resource.uri.clone(),
                mime_type: resource.mime_type.clone(),
                name: resource.name.clone(),
                size_bytes: resource.size_bytes,
            },
        })
        .collect()
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}
impl From<&Usage> for HistoryUsage {
    fn from(value: &Usage) -> Self {
        Self {
            input_tokens: value.input_tokens,
            output_tokens: value.output_tokens,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryMessage {
    pub id: String,
    pub run_id: RunId,
    pub step_id: Option<String>,
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<MessageProvenance>,
    pub content: Vec<HistoryContentBlock>,
    pub structured: Option<Value>,
    pub status: HistoryStatus,
    pub created_at_ms: u64,
}
impl HistoryMessage {
    pub fn with_provenance(mut self, provenance: Option<MessageProvenance>) -> Self {
        self.provenance = provenance;
        self
    }
    pub fn user(run_id: RunId, content: &Content) -> Self {
        Self {
            id: RunId::new_v4().to_string(),
            run_id,
            step_id: None,
            role: Role::User,
            provenance: None,
            content: history_content(content),
            structured: None,
            status: HistoryStatus::Completed,
            created_at_ms: unix_time_ms(),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryStep {
    pub id: String,
    pub run_id: RunId,
    pub node_invocation_id: Option<RunId>,
    pub sequence: usize,
    pub attempt: usize,
    pub name: String,
    pub model: Option<ModelRef>,
    pub status: HistoryStatus,
    pub usage: Option<HistoryUsage>,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryToolInvocation {
    pub id: RunId,
    pub run_id: RunId,
    pub step_id: String,
    pub message_id: String,
    pub call_id: String,
    pub tool_id: String,
    pub tool_name: String,
    pub arguments: Value,
    pub status: HistoryStatus,
    pub content: Vec<HistoryContentBlock>,
    pub structured: Option<Value>,
    pub is_error: Option<bool>,
    pub result_reference: Option<HistoryContentBlock>,
    pub started_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryInteraction {
    pub id: RunId,
    pub run_id: RunId,
    pub invocation_id: Option<RunId>,
    pub prompt: Vec<HistoryContentBlock>,
    pub response_schema: Value,
    pub presentation: Option<Value>,
    pub response: Option<Value>,
    /// Sender of the committed answer, independent of the original run initiator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_provenance: Option<MessageProvenance>,
    pub expires_at_ms: Option<u64>,
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryRecords {
    pub steps: Vec<HistoryStep>,
    pub messages: Vec<HistoryMessage>,
    pub tool_invocations: Vec<HistoryToolInvocation>,
    pub interactions: Vec<HistoryInteraction>,
}
impl HistoryRecords {
    pub fn validate(&self, run_id: RunId) -> Result<(), StateError> {
        let mut ids = std::collections::HashSet::new();
        for step in &self.steps {
            if step.id.is_empty() || step.run_id != run_id || !ids.insert(&step.id) {
                return Err(StateError::Invalid("invalid history step identity".into()));
            }
        }
        ids.clear();
        for message in &self.messages {
            if message.id.is_empty()
                || message.run_id != run_id
                || !matches!(message.role, Role::User | Role::Assistant)
                || !ids.insert(&message.id)
                || message
                    .step_id
                    .as_ref()
                    .is_some_and(|id| !self.steps.iter().any(|s| &s.id == id))
            {
                return Err(StateError::Invalid("invalid public history message".into()));
            }
        }
        let mut invocations = std::collections::HashSet::new();
        for tool in &self.tool_invocations {
            if tool.id.is_nil()
                || tool.run_id != run_id
                || !invocations.insert(tool.id)
                || !self
                    .messages
                    .iter()
                    .any(|m| m.id == tool.message_id && m.step_id.as_ref() == Some(&tool.step_id))
            {
                return Err(StateError::Invalid(
                    "invalid history tool invocation".into(),
                ));
            }
        }
        invocations.clear();
        for interaction in &self.interactions {
            if interaction.id.is_nil()
                || interaction.run_id != run_id
                || !invocations.insert(interaction.id)
                || (interaction.response.is_none() && interaction.response_provenance.is_some())
            {
                return Err(StateError::Invalid("invalid history interaction".into()));
            }
        }
        Ok(())
    }
    pub fn validate_successor(&self, next: &Self) -> Result<(), StateError> {
        for old in &self.messages {
            let new = next
                .messages
                .iter()
                .find(|m| m.id == old.id)
                .ok_or_else(|| StateError::Invalid("history messages cannot be removed".into()))?;
            if old.run_id != new.run_id
                || old.role != new.role
                || old.provenance != new.provenance
                || old.step_id != new.step_id
                || old.created_at_ms != new.created_at_ms
                || (old.status != HistoryStatus::Running && old != new)
            {
                return Err(StateError::Invalid(
                    "finished history messages are immutable".into(),
                ));
            }
        }
        for old in &self.tool_invocations {
            let new = next
                .tool_invocations
                .iter()
                .find(|t| t.id == old.id)
                .ok_or_else(|| {
                    StateError::Invalid("history invocations cannot be removed".into())
                })?;
            if old.run_id != new.run_id
                || old.step_id != new.step_id
                || old.message_id != new.message_id
                || old.call_id != new.call_id
                || (old.status != HistoryStatus::Planned
                    && (old.tool_id != new.tool_id || old.tool_name != new.tool_name))
                || old.arguments != new.arguments
                || (old.is_error.is_some() && old != new)
            {
                return Err(StateError::Invalid(
                    "history invocation identity/result is immutable".into(),
                ));
            }
        }
        for old in &self.steps {
            let new =
                next.steps.iter().find(|s| s.id == old.id).ok_or_else(|| {
                    StateError::Invalid("history attempts cannot be removed".into())
                })?;
            if old.run_id != new.run_id
                || old.sequence != new.sequence
                || old.attempt != new.attempt
                || old.started_at_ms != new.started_at_ms
                || (old.status != HistoryStatus::Running && old != new)
            {
                return Err(StateError::Invalid(
                    "history attempt identity/result is immutable".into(),
                ));
            }
        }
        for old in &self.interactions {
            let new = next
                .interactions
                .iter()
                .find(|i| i.id == old.id)
                .ok_or_else(|| {
                    StateError::Invalid("history interactions cannot be removed".into())
                })?;
            if old.run_id != new.run_id
                || old.invocation_id != new.invocation_id
                || old.prompt != new.prompt
                || old.response_schema != new.response_schema
                || old.presentation != new.presentation
                || old.expires_at_ms != new.expires_at_ms
                || (old.response.is_some() && old.response != new.response)
                || (old.response_provenance.is_some()
                    && old.response_provenance != new.response_provenance)
            {
                return Err(StateError::Invalid(
                    "history interaction identity/answer is immutable".into(),
                ));
            }
        }
        Ok(())
    }
    /// Merge one embedded execution into its parent's atomic commit.
    pub fn merge(&mut self, other: &Self) {
        macro_rules! merge {
            ($field:ident) => {
                for record in &other.$field {
                    if let Some(old) = self.$field.iter_mut().find(|r| r.id == record.id) {
                        *old = record.clone();
                    } else {
                        self.$field.push(record.clone());
                    }
                }
            };
        }
        merge!(steps);
        merge!(messages);
        merge!(tool_invocations);
        merge!(interactions);
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryRun {
    pub id: RunId,
    pub session_id: String,
    pub parent_run_id: Option<RunId>,
    pub root_run_id: RunId,
    pub status: HistoryStatus,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub error: Option<String>,
}
/// Cursor binds a session and a version. A concurrent mutation invalidates it;
/// clients reload the newest page instead of silently mixing read versions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryCursor {
    pub session_id: String,
    pub version: String,
    pub before: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryQuery {
    pub limit: usize,
    pub cursor: Option<HistoryCursor>,
}
impl Default for HistoryQuery {
    fn default() -> Self {
        Self {
            limit: 20,
            cursor: None,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHistory {
    pub session_id: String,
    pub version: String,
    pub next_cursor: Option<HistoryCursor>,
    pub runs: Vec<HistoryRun>,
    pub records: HistoryRecords,
    pub pending_interactions: Vec<HistoryInteraction>,
    pub active_runs: Vec<HistoryRun>,
}
#[async_trait]
pub trait HistoryStore: Send + Sync {
    /// Metadata only; does not read messages, tools or recovery checkpoints.
    async fn run(&self, _run_id: RunId) -> Result<HistoryRun, StateError> {
        Err(StateError::Invalid(
            "history run summary query is unavailable".into(),
        ))
    }
    /// Reads only the requested session. Lineage is metadata, not permission to
    /// merge another session's transcript into this one.
    async fn read_session(
        &self,
        session_id: &str,
        query: HistoryQuery,
    ) -> Result<SessionHistory, StateError>;
}
pub struct HistoryStoreService(pub Arc<dyn HistoryStore>);
#[derive(Clone)]
pub struct HistoryService(pub Arc<dyn HistoryStore>);
impl HistoryService {
    pub async fn run(&self, run_id: RunId) -> Result<HistoryRun, StateError> {
        self.0.run(run_id).await
    }
    pub async fn read_session(
        &self,
        session_id: &str,
        query: HistoryQuery,
    ) -> Result<SessionHistory, StateError> {
        self.0.read_session(session_id, query).await
    }
}
/// Execution-local writer sharing the checkpoint's fencing and revision lock.
#[doc(hidden)]
#[async_trait]
pub trait HistoryCommitter: Send + Sync {
    async fn model_attempt(
        &self,
        step: HistoryStep,
        message: HistoryMessage,
        response: Option<ChatResponse>,
    ) -> Result<(), StateError>;
}
#[doc(hidden)]
#[derive(Clone)]
pub struct HistoryCommitterService(pub Arc<dyn HistoryCommitter>);
