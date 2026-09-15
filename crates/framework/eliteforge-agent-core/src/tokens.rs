//! ChatModel-aware estimates used consistently for projection and final admission.
use crate::*;

/// Host/provider supplied token accounting. Implementations may use a tokenizer
/// and model-specific media costs. Include protocol framing and opaque model
/// continuation; unknown or unbounded content should return usize::MAX.
pub trait TokenEstimator: Send + Sync {
    fn message(&self, model: &ModelRef, message: &Message) -> usize;
    fn tools(&self, _model: &ModelRef, tools: &[ToolDescriptor]) -> usize {
        tools.iter().fold(0usize, |sum, tool| {
            sum.saturating_add(
                serde_json::to_vec(tool).map_or(usize::MAX, |v| v.len().saturating_add(16)),
            )
        })
    }
    fn options(&self, _model: &ModelRef, options: &GenerationOptions) -> usize {
        options
            .output_schema
            .as_ref()
            .map_or(0, |s| serde_json::to_vec(s).map_or(usize::MAX, |v| v.len()))
    }
    fn request(&self, model: &ModelRef, request: &ChatRequest) -> usize {
        request
            .messages
            .iter()
            .fold(0usize, |sum, m| sum.saturating_add(self.message(model, m)))
            .saturating_add(self.tools(model, &request.tools))
            .saturating_add(self.options(model, &request.options))
    }
}

/// Safe default when no tokenizer/media estimator is supplied; bytes are an
/// intentionally conservative estimate and are not presented as billed tokens.
pub struct ConservativeTokenEstimator;
impl TokenEstimator for ConservativeTokenEstimator {
    fn message(&self, _: &ModelRef, message: &Message) -> usize {
        estimate_message_tokens(message)
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
