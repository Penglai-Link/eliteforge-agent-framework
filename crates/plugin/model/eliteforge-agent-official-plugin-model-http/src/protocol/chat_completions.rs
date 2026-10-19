use serde_json::{Value, json};

use crate::{
    AgentMessage, AgentMessagePart, AgentModelConfig, AgentModelProtocol, ModelFinish,
    ModelRequest, ModelResponse, ModelStreamEvent, ModelTokenUsage, ToolCall,
};

use super::{content::*, registry::ModelProtocolAdapter, usage::parse_openai_compatible_usage};

#[derive(Debug, Clone, Copy)]
pub struct ChatCompletionsAdapter;

impl ModelProtocolAdapter for ChatCompletionsAdapter {
    fn protocol(&self) -> AgentModelProtocol {
        AgentModelProtocol::ChatCompletions
    }

    fn default_path(&self) -> &'static str {
        "chat/completions"
    }

    fn request_body(
        &self,
        config: &AgentModelConfig,
        request: &ModelRequest,
    ) -> Result<Value, String> {
        Ok(chat_completions_request(config, request))
    }

    fn stream_request_body(
        &self,
        config: &AgentModelConfig,
        request: &ModelRequest,
    ) -> Result<Value, String> {
        let mut body = chat_completions_request(config, request);
        body["stream"] = json!(true);
        body["stream_options"] = json!({ "include_usage": true });
        Ok(body)
    }

    fn parse_response(&self, body: &Value) -> Result<ModelResponse, String> {
        parse_chat_response(body).ok_or_else(|| unsupported_response(body))
    }

    fn parse_stream_event(&self, body: &Value) -> Result<Option<ModelStreamEvent>, String> {
        Ok(parse_chat_stream_event(body))
    }
    fn parse_stream_events(&self, body: &Value) -> Result<Vec<ModelStreamEvent>, String> {
        let Some(delta) = body.pointer("/choices/0/delta") else {
            return Ok(Vec::new());
        };
        // Compatible providers can return text and reasoning in one packet.
        // They are independent channels, not mutually exclusive alternatives.
        Ok([
            chat_reasoning_delta(delta).map(ModelStreamEvent::reasoning_delta),
            chat_compatible_reasoning_summary(delta).map(ModelStreamEvent::reasoning_summary_delta),
            delta
                .get("content")
                .and_then(text_from_content)
                .map(ModelStreamEvent::text_delta),
        ]
        .into_iter()
        .flatten()
        .filter(|event| !event.delta.is_empty())
        .collect())
    }

    fn parse_usage(&self, body: &Value) -> Option<ModelTokenUsage> {
        parse_openai_compatible_usage(body)
    }
}

/// Only protocol shape and numeric usage enter diagnostics. Provider content,
/// reasoning, identifiers and arbitrary finish/error strings may contain secrets.
fn unsupported_response(body: &Value) -> String {
    let choice = body.pointer("/choices/0");
    let message = choice.and_then(|value| value.get("message"));
    let finish = match choice
        .and_then(|value| value.get("finish_reason"))
        .and_then(Value::as_str)
    {
        Some("length") => "Length",
        Some("stop") => "Stop",
        Some("tool_calls") => "ToolCalls",
        Some("function_call") => "FunctionCall",
        Some("content_filter") => "ContentFilter",
        Some(_) => "Other",
        None => "Missing",
    };
    let content = match message.and_then(|value| value.get("content")) {
        Some(Value::String(text)) if text.is_empty() => "EmptyString",
        Some(Value::String(_)) => "String",
        Some(Value::Array(_)) => "Array",
        Some(Value::Null) => "Null",
        Some(_) => "Other",
        None => "Missing",
    };
    let reasoning = message.is_some_and(|value| {
        ["reasoning_content", "reasoning"]
            .iter()
            .any(|key| value.get(key).is_some_and(|part| !part.is_null()))
    });
    let choices = body.get("choices").and_then(Value::as_array).map(Vec::len);
    let completion = body
        .pointer("/usage/completion_tokens")
        .and_then(Value::as_u64);
    let reasoning_tokens = body
        .pointer("/usage/completion_tokens_details/reasoning_tokens")
        .and_then(Value::as_u64);
    format!(
        "model response was unsupported (finish={finish}; choices={choices:?}; content={content}; reasoning={reasoning}; completionTokens={completion:?}; reasoningTokens={reasoning_tokens:?})"
    )
}

pub fn chat_completions_request(config: &AgentModelConfig, request: &ModelRequest) -> Value {
    let mut payload = json!({
        "messages": request.messages.iter().map(chat_message_json).collect::<Vec<_>>(),
    });
    // Routes without a physical model let the provider select one. Omit the
    // model field for those routes instead of sending an empty model name.
    if !config.model_name.trim().is_empty() {
        payload["model"] = json!(config.model_name);
    }
    if let Some(temperature) = config.temperature {
        payload["temperature"] = json!(temperature);
    }
    if let Some(max_tokens) = config.max_tokens {
        payload["max_tokens"] = json!(max_tokens);
    }
    if let Some(top_p) = config.top_p {
        payload["top_p"] = json!(top_p);
    }
    if config.thinking_enabled {
        payload["enable_thinking"] = json!(true);
        payload["thinking"] = json!({ "type": "enabled" });
        if let Some(reasoning_effort) = config.reasoning_effort {
            payload["reasoning_effort"] = json!(reasoning_effort.provider_value());
        }
    }
    if !request.tools.is_empty() {
        payload["tools"] =
            crate::request_tool_schema_payload(AgentModelProtocol::ChatCompletions, request);
        payload["parallel_tool_calls"] = json!(true);
    } else {
        // Plain generation must not inherit provider-side/default tools.
        payload["tool_choice"] = json!("none");
    }
    payload
}

fn parse_chat_response(body: &Value) -> Option<ModelResponse> {
    let token_usage = parse_openai_compatible_usage(body);
    let response_id = body.get("id").and_then(Value::as_str).map(str::to_string);
    let message = body.get("choices")?.as_array()?.first()?.get("message")?;
    let mut parts = Vec::new();
    if let Some(content) = message.get("content").and_then(text_from_content) {
        parts.push(AgentMessagePart::Text { text: content });
    }
    if let Some(summary) = chat_compatible_reasoning_summary(message) {
        parts.push(AgentMessagePart::ReasoningSummary { text: summary });
    }
    if let Some(tool_calls) = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| parse_chat_tool_calls(calls.as_slice()))
        .filter(|calls| !calls.is_empty())
    {
        parts.extend(tool_calls.into_iter().map(tool_call_part));
        return Some(ModelResponse::from_assistant(
            AgentMessage::new("assistant", parts),
            ModelFinish::ToolCalls,
            token_usage,
            None,
            response_id,
        ));
    }
    if let Some(function_call) = message.get("function_call") {
        let tool_call = parse_chat_legacy_function_call(function_call)?;
        parts.push(tool_call_part(tool_call));
        return Some(ModelResponse::from_assistant(
            AgentMessage::new("assistant", parts),
            ModelFinish::ToolCalls,
            token_usage,
            None,
            response_id,
        ));
    }
    // Raw reasoning is not an answer, even when the provider returns no content.
    (!parts.is_empty()).then(|| {
        ModelResponse::from_assistant(
            AgentMessage::new("assistant", parts),
            ModelFinish::Final,
            token_usage,
            None,
            response_id,
        )
    })
}

fn parse_chat_tool_calls(calls: &[Value]) -> Vec<ToolCall> {
    calls
        .iter()
        .filter_map(parse_chat_tool_call)
        .collect::<Vec<_>>()
}

fn parse_chat_tool_call(call: &Value) -> Option<ToolCall> {
    let function = call.get("function")?;
    let name = function.get("name")?.as_str()?.to_owned();
    let call_id = call
        .get("call_id")
        .or_else(|| call.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let (input, input_error) = parse_chat_tool_call_arguments(function);
    Some(ToolCall {
        call_id,
        tool_name: name,
        input,
        input_error,
    })
}

fn parse_chat_legacy_function_call(function_call: &Value) -> Option<ToolCall> {
    let name = function_call.get("name")?.as_str()?;
    let (input, input_error) = parse_chat_tool_call_arguments(function_call);
    Some(ToolCall {
        call_id: None,
        tool_name: name.to_owned(),
        input,
        input_error,
    })
}

fn tool_call_part(call: ToolCall) -> AgentMessagePart {
    AgentMessagePart::ToolCall {
        call_id: call.call_id,
        tool_name: call.tool_name,
        input: call.input,
        input_error: call.input_error,
    }
}

fn parse_chat_tool_call_arguments(function: &Value) -> (Value, Option<String>) {
    let Some(raw_arguments) = function.get("arguments").and_then(Value::as_str) else {
        return (
            Value::Null,
            Some("function arguments are missing or are not a JSON string".into()),
        );
    };
    match serde_json::from_str(raw_arguments) {
        Ok(input) => (input, None),
        Err(error) => (
            Value::Null,
            Some(format!("function arguments are invalid JSON: {error}")),
        ),
    }
}

fn parse_chat_stream_event(body: &Value) -> Option<ModelStreamEvent> {
    let delta = body.get("choices")?.as_array()?.first()?.get("delta")?;
    // OpenAI Chat Completions defines no structured reasoning-summary field,
    // but compatible providers may expose an explicit summary extension.
    let event = if let Some(text) = delta.get("content").and_then(text_from_content) {
        ModelStreamEvent::text_delta(text)
    } else if let Some(text) = chat_compatible_reasoning_summary(delta) {
        ModelStreamEvent::reasoning_summary_delta(text)
    } else if let Some(text) = chat_reasoning_delta(delta) {
        ModelStreamEvent::reasoning_delta(text)
    } else {
        return None;
    };
    if event.delta.is_empty() {
        None
    } else {
        Some(event)
    }
}

fn chat_compatible_reasoning_summary(delta: &Value) -> Option<String> {
    [
        "reasoning_summary_content",
        "reasoning_summary",
        "thinking_summary_content",
        "thinking_summary",
    ]
    .iter()
    .find_map(|key| delta.get(key).and_then(text_from_content))
}

fn chat_reasoning_delta(delta: &Value) -> Option<String> {
    [
        "reasoning_content",
        "reasoning",
        "thinking_content",
        "thinking",
    ]
    .iter()
    .find_map(|key| delta.get(key).and_then(text_from_content))
}

#[cfg(test)]
mod history_stream_tests {
    use super::*;
    #[test]
    fn retains_every_channel_in_one_compatible_provider_packet() {
        let events=ChatCompletionsAdapter.parse_stream_events(&serde_json::json!({"choices":[{"delta":{"content":"answer","reasoning_content":"actual reasoning","reasoning_summary":"summary"}}]})).unwrap();
        assert_eq!(
            events,
            vec![
                ModelStreamEvent::reasoning_delta("actual reasoning"),
                ModelStreamEvent::reasoning_summary_delta("summary"),
                ModelStreamEvent::text_delta("answer")
            ]
        );
    }
}
