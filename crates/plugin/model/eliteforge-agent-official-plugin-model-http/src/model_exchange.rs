use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{AgentMessage, AgentMessagePart, EffectiveTool, PreparedToolSchema};

const GENERATED_TOOL_CALL_ID_PREFIX: &str = "generated_call_";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRequest {
    pub messages: Vec<AgentMessage>,
    pub tools: Vec<EffectiveTool>,
    /// Run-local cache; never part of the serialized request contract.
    #[serde(skip)]
    pub prepared_tool_schema: Option<std::sync::Arc<PreparedToolSchema>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_agent_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_input_items: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
}

impl PartialEq for ModelRequest {
    fn eq(&self, other: &Self) -> bool {
        self.messages == other.messages
            && self.tools == other.tools
            && self.system_agent_session_id == other.system_agent_session_id
            && self.response_input_items == other.response_input_items
            && self.previous_response_id == other.previous_response_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    pub tool_name: String,
    pub input: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_error: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelTokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub total_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenEstimateSnapshot {
    pub estimated_input_tokens: u32,
    pub message_token_estimate: u32,
    pub tool_schema_token_estimate: u32,
    pub context_window_limit: Option<u32>,
    pub effective_input_budget: Option<u32>,
    pub output_token_reserve: Option<u32>,
    pub rebased: bool,
    pub over_budget: bool,
    pub compaction_trigger_reached: bool,
    pub checkpoint_code: Option<String>,
    pub model_code: String,
    pub model_name: String,
    pub model_protocol: String,
    pub usage_source: String,
    pub usage_confidence: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsageAuditSnapshot {
    pub request_estimate: TokenEstimateSnapshot,
    pub provider_usage: Option<ModelTokenUsage>,
    pub cumulative_run_usage: Option<ModelTokenUsage>,
    pub input_token_delta: Option<i64>,
    pub input_token_ratio: Option<f64>,
    pub calibration_factor: Option<f64>,
    pub response_id: Option<String>,
    pub usage_source: String,
    pub usage_confidence: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ModelFinish {
    Final,
    ToolCalls,
    Continue,
}

/// Ordered provider output, kept separate from the legacy assistant projection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ModelOutputItem {
    Message {
        output_index: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        phase: Option<String>,
        text: String,
    },
    ReasoningSummary { output_index: usize, text: String },
    ToolCall { output_index: usize, call: ToolCall },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelResponse {
    pub assistant: AgentMessage,
    pub finish: ModelFinish,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_items: Vec<ModelOutputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_usage: Option<ModelTokenUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_agent_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelStreamEvent {
    pub kind: ModelStreamEventKind,
    pub delta: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ModelStreamEventKind {
    Text,
    Reasoning,
    ReasoningSummary,
    MessageStart,
    MessageEnd,
    ToolBoundary,
    ToolCallDelta,
}

impl ModelStreamEvent {
    /// Generated tool name/argument fragments; never assistant-visible text.
    pub fn tool_call_delta(delta: impl Into<String>) -> Self {
        Self { kind: ModelStreamEventKind::ToolCallDelta, delta: delta.into(), output_index: None, item_id: None, phase: None }
    }

    pub fn text_delta(delta: impl Into<String>) -> Self {
        Self {
            kind: ModelStreamEventKind::Text,
            delta: delta.into(),
            output_index: None,
            item_id: None,
            phase: None,
        }
    }

    pub fn reasoning_delta(delta: impl Into<String>) -> Self {
        Self {
            kind: ModelStreamEventKind::Reasoning,
            delta: delta.into(),
            output_index: None,
            item_id: None,
            phase: None,
        }
    }

    pub fn reasoning_summary_delta(delta: impl Into<String>) -> Self {
        Self {
            kind: ModelStreamEventKind::ReasoningSummary,
            delta: delta.into(),
            output_index: None,
            item_id: None,
            phase: None,
        }
    }
}

impl ModelResponse {
    pub fn from_assistant(
        mut assistant: AgentMessage,
        finish: ModelFinish,
        token_usage: Option<ModelTokenUsage>,
        system_agent_session_id: Option<String>,
        response_id: Option<String>,
    ) -> Self {
        if finish == ModelFinish::ToolCalls {
            ensure_tool_call_ids(&mut assistant);
        }
        Self {
            assistant,
            finish,
            output_items: Vec::new(),
            token_usage,
            system_agent_session_id,
            response_id,
        }
    }

    pub fn final_output(output: impl Into<String>) -> Self {
        Self::final_output_with_usage_and_response_id(output, None, None)
    }

    pub fn final_output_with_usage(
        output: impl Into<String>,
        token_usage: Option<ModelTokenUsage>,
    ) -> Self {
        Self::final_output_with_usage_and_session_and_response_id(output, token_usage, None, None)
    }

    pub fn final_output_with_usage_and_session(
        output: impl Into<String>,
        token_usage: Option<ModelTokenUsage>,
        system_agent_session_id: Option<String>,
    ) -> Self {
        Self::final_output_with_usage_and_session_and_response_id(
            output,
            token_usage,
            system_agent_session_id,
            None,
        )
    }

    pub fn final_output_with_usage_and_response_id(
        output: impl Into<String>,
        token_usage: Option<ModelTokenUsage>,
        response_id: Option<String>,
    ) -> Self {
        Self::final_output_with_usage_and_session_and_response_id(
            output,
            token_usage,
            None,
            response_id,
        )
    }

    pub fn final_output_with_usage_and_session_and_response_id(
        output: impl Into<String>,
        token_usage: Option<ModelTokenUsage>,
        system_agent_session_id: Option<String>,
        response_id: Option<String>,
    ) -> Self {
        Self::from_assistant(
            AgentMessage::assistant(output),
            ModelFinish::Final,
            token_usage,
            system_agent_session_id,
            response_id,
        )
    }

    pub fn tool_call(tool_name: impl Into<String>, input: Value) -> Self {
        Self::tool_call_with_usage_and_response_id(tool_name, input, None, None)
    }

    pub fn tool_call_with_id(
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        input: Value,
    ) -> Self {
        Self::tool_calls_with_usage_and_response_id(
            vec![ToolCall {
                call_id: Some(call_id.into()),
                tool_name: tool_name.into(),
                input,
                input_error: None,
            }],
            None,
            None,
        )
    }

    pub fn from_tool_calls(calls: Vec<ToolCall>) -> Self {
        Self::tool_calls_with_usage_and_response_id(calls, None, None)
    }

    pub fn tool_calls_with_usage(
        calls: Vec<ToolCall>,
        token_usage: Option<ModelTokenUsage>,
    ) -> Self {
        Self::tool_calls_with_usage_and_response_id(calls, token_usage, None)
    }

    pub fn tool_calls_with_usage_and_response_id(
        calls: Vec<ToolCall>,
        token_usage: Option<ModelTokenUsage>,
        response_id: Option<String>,
    ) -> Self {
        Self::from_assistant(
            AgentMessage::new(
                "assistant",
                calls
                    .into_iter()
                    .map(|call| AgentMessagePart::ToolCall {
                        call_id: call.call_id,
                        tool_name: call.tool_name,
                        input: call.input,
                        input_error: call.input_error,
                    })
                    .collect(),
            ),
            ModelFinish::ToolCalls,
            token_usage,
            None,
            response_id,
        )
    }

    pub fn tool_call_with_usage(
        tool_name: impl Into<String>,
        input: Value,
        token_usage: Option<ModelTokenUsage>,
    ) -> Self {
        Self::tool_call_with_usage_and_response_id(tool_name, input, token_usage, None)
    }

    pub fn tool_call_with_usage_and_response_id(
        tool_name: impl Into<String>,
        input: Value,
        token_usage: Option<ModelTokenUsage>,
        response_id: Option<String>,
    ) -> Self {
        Self::tool_calls_with_usage_and_response_id(
            vec![ToolCall {
                call_id: None,
                tool_name: tool_name.into(),
                input,
                input_error: None,
            }],
            token_usage,
            response_id,
        )
    }

    pub fn token_usage(&self) -> Option<ModelTokenUsage> {
        self.token_usage
    }

    pub fn response_id(&self) -> Option<&str> {
        self.response_id.as_deref()
    }

    pub fn system_agent_session_id(&self) -> Option<&str> {
        self.system_agent_session_id.as_deref()
    }

    pub fn output_text(&self) -> String {
        if self.output_items.is_empty() {
            return self.assistant.output_text_content();
        }
        let has_final = self.output_items.iter().any(|item| matches!(item,
            ModelOutputItem::Message { phase: Some(phase), text, .. }
                if phase == "final_answer" && !text.trim().is_empty()));
        self.output_items.iter().filter_map(|item| match item {
            ModelOutputItem::Message { phase, text, .. }
                if !text.is_empty() && (phase.as_deref() == Some("final_answer")
                    || (!has_final && phase.is_none())) => Some(text.as_str()),
            _ => None,
        }).collect::<Vec<_>>().join("\n\n")
    }

    pub fn tool_calls(&self) -> Vec<ToolCall> {
        self.assistant
            .parts
            .iter()
            .filter_map(|part| match part {
                AgentMessagePart::ToolCall {
                    call_id,
                    tool_name,
                    input,
                    input_error,
                } => Some(ToolCall {
                    call_id: call_id.clone(),
                    tool_name: tool_name.clone(),
                    input: input.clone(),
                    input_error: input_error.clone(),
                }),
                _ => None,
            })
            .collect()
    }
}

fn ensure_tool_call_ids(assistant: &mut AgentMessage) {
    let mut call_index = 0usize;
    for part in &mut assistant.parts {
        if let AgentMessagePart::ToolCall {
            call_id,
            tool_name,
            input,
            ..
        } = part
        {
            let missing = call_id.as_ref().is_none_or(|value| value.trim().is_empty());
            if missing {
                *call_id = Some(stable_tool_call_id(tool_name, input, call_index));
            }
            call_index = call_index.saturating_add(1);
        }
    }
}

fn stable_tool_call_id(tool_name: &str, input: &Value, call_index: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(tool_name.as_bytes());
    hasher.update(b"\0");
    hasher.update(input.to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(call_index.to_string().as_bytes());
    let digest = hex::encode(hasher.finalize());
    format!("{GENERATED_TOOL_CALL_ID_PREFIX}{}", &digest[..16])
}

pub(crate) fn is_generated_tool_call_id(call_id: &str) -> bool {
    call_id.starts_with(GENERATED_TOOL_CALL_ID_PREFIX)
}

impl ModelTokenUsage {
    pub fn merge(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            total_tokens: self.total_tokens.saturating_add(other.total_tokens),
            cached_input_tokens: add_optional_tokens(
                self.cached_input_tokens,
                other.cached_input_tokens,
            ),
            reasoning_tokens: add_optional_tokens(self.reasoning_tokens, other.reasoning_tokens),
        }
    }
}

fn add_optional_tokens(left: Option<u32>, right: Option<u32>) -> Option<u32> {
    match (left, right) {
        (None, None) => None,
        (left, right) => Some(left.unwrap_or(0).saturating_add(right.unwrap_or(0))),
    }
}

pub(crate) fn responses_input_item_json(message: &AgentMessage) -> Value {
    match message.role.as_str() {
        "system" => json!({
            "type": "message",
            "role": message.role.as_str(),
            "content": message.plain_text_content(),
        }),
        "user" | "assistant" => {
            let mut item = json!({
                "type": "message",
                "role": message.role.as_str(),
                "content": responses_message_content(message),
            });
            if message.role == "assistant" && let Some(phase) = &message.phase {
                item["phase"] = json!(phase);
            }
            item
        },
        "tool" => match message.parts.iter().find_map(tool_result_part) {
            Some((Some(call_id), output)) => {
                json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": output,
                })
            }
            _ => json!({
                "type": "message",
                "role": "user",
                "content": message.plain_text_content(),
            }),
        },
        _ => json!({
            "type": "message",
            "role": "user",
            "content": message.plain_text_content(),
        }),
    }
}

fn responses_message_content(message: &AgentMessage) -> Value {
    let image_parts = message
        .parts
        .iter()
        .filter_map(responses_image_content_part)
        .collect::<Vec<_>>();
    if message.role != "user" || image_parts.is_empty() {
        return json!(message.plain_text_content());
    }
    let mut content = Vec::new();
    let text = message.plain_text_content();
    if !text.trim().is_empty() {
        content.push(json!({
            "type": "input_text",
            "text": text,
        }));
    }
    content.extend(image_parts);
    Value::Array(content)
}

fn responses_image_content_part(part: &AgentMessagePart) -> Option<Value> {
    let AgentMessagePart::Image { mime_type, data } = part else {
        return None;
    };
    Some(json!({
        "type": "input_image",
        "image_url": if data.starts_with("http://") || data.starts_with("https://") || data.starts_with("data:") {data.clone()} else {format!("data:{mime_type};base64,{data}")},
    }))
}

fn tool_result_part(part: &AgentMessagePart) -> Option<(Option<&str>, &str)> {
    match part {
        AgentMessagePart::ToolResult {
            call_id, output, ..
        } => Some((call_id.as_deref(), output.as_str())),
        _ => None,
    }
}

pub(crate) fn responses_function_call_item_json(part: &AgentMessagePart) -> Option<Value> {
    match part {
        AgentMessagePart::ToolCall {
            call_id,
            tool_name,
            input,
            ..
        } => Some(json!({
            "type": "function_call",
            "call_id": call_id,
            "name": tool_name,
            "arguments": function_call_arguments_json(input),
        })),
        AgentMessagePart::ToolResult {
            call_id: Some(call_id),
            output,
            ..
        } => Some(json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": output,
        })),
        _ => None,
    }
}

/// A malformed provider tool call is retained in the transcript so that the
/// model can receive its failed observation and recover. Some OpenAI-compatible
/// providers reject `"null"` for function arguments, even though it is valid
/// JSON, so replay it as an empty object instead.
pub(crate) fn function_call_arguments_json(input: &Value) -> String {
    if input.is_null() {
        "{}".into()
    } else {
        input.to_string()
    }
}
