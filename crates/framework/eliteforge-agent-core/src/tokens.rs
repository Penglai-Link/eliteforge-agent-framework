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

/// Local Unicode-aware approximation for text; never used as billed usage.
/// ASCII word runs use three characters per token, CJK uses two tokens per
/// character, and punctuation/other scripts retain conservative individual costs.
/// This is intentionally independent of UTF-8 byte width and needs no model call.
pub fn estimate_unicode_text_tokens(text: &str) -> usize {
    let mut tokens = 0usize;
    let mut ascii_run = 0usize;
    let flush = |tokens: &mut usize, run: &mut usize| {
        *tokens = tokens.saturating_add(run.div_ceil(3));
        *run = 0;
    };
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            ascii_run = ascii_run.saturating_add(1);
            continue;
        }
        flush(&mut tokens, &mut ascii_run);
        let cost = if ch.is_ascii() {
            1
        } else if ch.is_alphanumeric() {
            2
        } else {
            3
        };
        tokens = tokens.saturating_add(cost);
    }
    flush(&mut tokens, &mut ascii_run);
    tokens
}
fn unicode_json_tokens<T: serde::Serialize>(value: &T) -> usize {
    serde_json::to_string(value).map_or(usize::MAX, |text| estimate_unicode_text_tokens(&text))
}

/// Text-aware admission accounting with explicit media estimates and opaque
/// resource bounds preserved. Unknown media remains unbounded.
pub struct UnicodeTokenEstimator;
#[async_trait::async_trait]
impl TokenEstimator for UnicodeTokenEstimator {
    async fn message(&self, _: &ModelRef, message: &Message) -> Result<usize, ModelError> {
        let mut tokens = 16usize;
        for block in &message.content.0 {
            tokens = tokens.saturating_add(match block {
                ContentBlock::Text { text } => estimate_unicode_text_tokens(text),
                ContentBlock::Json { value } => unicode_json_tokens(value),
                ContentBlock::Media { media } => media.estimated_tokens.unwrap_or(usize::MAX),
                // Resource handles can contain opaque serialized provider data.
                ContentBlock::Resource { resource } => {
                    serde_json::to_vec(resource).map_or(usize::MAX, |bytes| bytes.len())
                }
            });
        }
        tokens = tokens
            .saturating_add(message.structured.as_ref().map_or(0, unicode_json_tokens))
            .saturating_add(message.tool_result.as_ref().map_or(0, unicode_json_tokens))
            .saturating_add(
                message
                    .model_context
                    .as_ref()
                    .map_or(0, unicode_json_tokens),
            );
        for call in &message.tool_calls {
            tokens = tokens
                .saturating_add(16)
                .saturating_add(estimate_unicode_text_tokens(&call.id))
                .saturating_add(estimate_unicode_text_tokens(&call.call_name))
                .saturating_add(unicode_json_tokens(&call.arguments));
        }
        Ok(tokens)
    }
    async fn tools(&self, _: &ModelRef, tools: &[ToolDescriptor]) -> Result<usize, ModelError> {
        Ok(tools.iter().fold(0usize, |tokens, tool| {
            tokens.saturating_add(serde_json::to_string(tool).map_or(usize::MAX, |text| {
                estimate_unicode_text_tokens(&text).saturating_add(16)
            }))
        }))
    }
    async fn options(
        &self,
        _: &ModelRef,
        options: &GenerationOptions,
    ) -> Result<usize, ModelError> {
        Ok(options
            .output_schema
            .as_ref()
            .map_or(0, unicode_json_tokens))
    }
}
