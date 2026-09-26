//! ChatModel-aware estimates used consistently for projection and final admission.
use crate::*;

/// Host/provider supplied token accounting. Implementations may use a tokenizer
/// and model-specific media costs. Include protocol framing and opaque model
/// continuation; unknown or unbounded content should return usize::MAX.
#[async_trait::async_trait]
pub trait TokenEstimator: Send + Sync {
    async fn message(&self, model: &ModelRef, message: &Message) -> Result<usize, ModelError>;
    async fn tools(
        &self,
        _model: &ModelRef,
        tools: &[ToolDescriptor],
    ) -> Result<usize, ModelError> {
        Ok(tools.iter().fold(0usize, |sum, tool| {
            sum.saturating_add(
                serde_json::to_vec(tool).map_or(usize::MAX, |v| v.len().saturating_add(16)),
            )
        }))
    }
    async fn options(
        &self,
        _model: &ModelRef,
        options: &GenerationOptions,
    ) -> Result<usize, ModelError> {
        Ok(options
            .output_schema
            .as_ref()
            .map_or(0, |s| serde_json::to_vec(s).map_or(usize::MAX, |v| v.len())))
    }
    async fn request(&self, model: &ModelRef, request: &ChatRequest) -> Result<usize, ModelError> {
        let mut tokens = 0usize;
        for message in &request.messages {
            tokens = tokens.saturating_add(self.message(model, message).await?);
        }
        Ok(tokens
            .saturating_add(self.tools(model, &request.tools).await?)
            .saturating_add(self.options(model, &request.options).await?))
    }
}

/// Conservative byte estimate, distinct from billed tokens.
pub struct ConservativeTokenEstimator;
#[async_trait::async_trait]
impl TokenEstimator for ConservativeTokenEstimator {
    async fn message(&self, _: &ModelRef, message: &Message) -> Result<usize, ModelError> {
        Ok(estimate_message_tokens(message))
    }
}

/// Deliberately conservative byte-based estimate; adapters can use a tokenizer
/// on each model connection. Each message also reserves protocol framing overhead.
pub fn estimate_message_tokens(message: &Message) -> usize {
    message
        .content
        .estimated_tokens()
        .saturating_add(message.structured.as_ref().map_or(0, |data| {
            serde_json::to_vec(data).map_or(usize::MAX, |bytes| bytes.len())
        }))
        .saturating_add(message.tool_result.as_ref().map_or(0, |data| {
            serde_json::to_vec(data).map_or(usize::MAX, |v| v.len())
        }))
        .saturating_add(message.model_context.as_ref().map_or(0, |data| {
            serde_json::to_vec(data).map_or(0, |bytes| bytes.len())
        }))
        .saturating_add(
            message
                .tool_calls
                .iter()
                .map(|call| {
                    call.id
                        .len()
                        .saturating_add(call.call_name.len())
                        .saturating_add(call.arguments.to_string().len())
                        .saturating_add(16)
                })
                .fold(0usize, usize::saturating_add),
        )
        .saturating_add(16)
}
