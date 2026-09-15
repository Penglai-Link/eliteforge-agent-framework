use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::{ConversationError, Message, Role};

/// Host-resolved identity. A branch is an independent append-only transcript.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConversationKey {
    pub session_id: String,
    pub branch: String,
}

impl ConversationKey {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            branch: "main".into(),
        }
    }
    pub fn branch(mut self, branch: impl Into<String>) -> Self {
        self.branch = branch.into();
        self
    }
    pub fn validate(&self) -> Result<(), ConversationError> {
        if self.session_id.trim().is_empty() || self.branch.trim().is_empty() {
            return Err(ConversationError::Other(
                "conversation and branch must be nonempty".into(),
            ));
        }
        Ok(())
    }
}

/// Message identity is the conversation identity plus its one-based sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationMessageId {
    pub conversation: ConversationKey,
    pub generation: String,
    pub sequence: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationSnapshot {
    pub key: ConversationKey,
    /// Stable for this transcript lifetime; changes when an expired transcript is recreated.
    pub generation: String,
    /// The last committed message sequence. Empty transcripts have revision 0.
    pub revision: u64,
    pub messages: Vec<Message>,
}

impl ConversationSnapshot {
    pub fn new(
        key: ConversationKey,
        generation: impl Into<String>,
        messages: Vec<Message>,
    ) -> Self {
        Self {
            key,
            generation: generation.into(),
            revision: messages.len() as u64,
            messages,
        }
    }
    pub fn message_id(&self, index: usize) -> Option<ConversationMessageId> {
        (index < self.messages.len()).then(|| ConversationMessageId {
            conversation: self.key.clone(),
            generation: self.generation.clone(),
            sequence: index as u64 + 1,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationOutcome {
    Completed,
    AwaitingInput,
    Stopped,
    Cancelled,
    Failed,
}

#[crate::extension_point(single, adapter = crate::ConversationStorePlugin)]
#[async_trait]
pub trait ConversationStore: Send + Sync {
    async fn open(
        &self,
        session_id: &str,
    ) -> Result<Box<dyn ConversationSession>, ConversationError>;
    async fn open_branch(
        &self,
        key: &ConversationKey,
    ) -> Result<Box<dyn ConversationSession>, ConversationError> {
        key.validate()?;
        if key.branch != "main" {
            return Err(ConversationError::Other(
                "store does not support branches".into(),
            ));
        }
        self.open(&key.session_id).await
    }
}

/// A run-local session lease. Implement Drop for mandatory cleanup: cancellation
/// can interrupt any async operation, including `finish`.
#[async_trait]
pub trait ConversationSession: Send {
    /// An immutable view of committed messages, without model-input trimming.
    async fn snapshot(&mut self) -> Result<ConversationSnapshot, ConversationError>;
    /// Batches containing tool calls must include their matching results.
    async fn append(&mut self, messages: &[Message]) -> Result<(), ConversationError>;
    /// Sessions are exclusive leases; the version check and append share that lease.
    async fn append_at(
        &mut self,
        revision: u64,
        messages: &[Message],
    ) -> Result<(), ConversationError> {
        if self.snapshot().await?.revision != revision {
            return Err(ConversationError::Conflict);
        }
        self.append(messages).await
    }

    async fn finish(&mut self, _outcome: ConversationOutcome) -> Result<(), ConversationError> {
        Ok(())
    }
}

pub fn validate_messages(messages: &[Message]) -> Result<(), ConversationError> {
    let mut pending = HashSet::new();
    for message in messages {
        if !message.tool_calls.is_empty() && message.role != Role::Assistant {
            return Err(ConversationError::Other(
                "only assistant messages may request tools".into(),
            ));
        }
        if message.tool_result.is_some() && message.role != Role::Tool {
            return Err(ConversationError::Other(
                "tool result metadata requires tool role".into(),
            ));
        }
        if message.role == Role::Tool {
            let id = message
                .tool_call_id
                .as_deref()
                .ok_or_else(|| ConversationError::Other("tool result without call ID".into()))?;
            if !pending.remove(id) {
                return Err(ConversationError::Other(
                    "orphan or duplicate tool result".into(),
                ));
            }
        } else {
            if !pending.is_empty() {
                return Err(ConversationError::Other(
                    "tool calls must be followed by their results".into(),
                ));
            }
            if message.tool_call_id.is_some() {
                return Err(ConversationError::Other(
                    "only tool results may carry tool_call_id".into(),
                ));
            }
            for call in &message.tool_calls {
                if call.id.is_empty() || !pending.insert(call.id.as_str()) {
                    return Err(ConversationError::Other(
                        "empty or duplicate tool call ID".into(),
                    ));
                }
            }
        }
    }
    if !pending.is_empty() {
        return Err(ConversationError::Other(
            "tool call batch has missing results".into(),
        ));
    }
    Ok(())
}
