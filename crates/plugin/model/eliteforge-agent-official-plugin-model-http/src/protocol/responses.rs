use serde_json::{Value, json};

use crate::{
    AgentMessage, AgentMessagePart, AgentModelConfig, AgentModelProtocol, ModelFinish,
    ModelRequest, ModelResponse, ModelStreamEvent, ModelTokenUsage, ToolCall,
    responses_function_call_item_json, responses_input_item_json,
};

use super::{content::*, registry::ModelProtocolAdapter, usage::parse_responses_usage};

#[derive(Debug, Clone, Copy)]
pub struct ResponsesAdapter;

impl ModelProtocolAdapter for ResponsesAdapter {
    fn protocol(&self) -> AgentModelProtocol {
        AgentModelProtocol::Responses
    }

    fn default_path(&self) -> &'static str {
        "responses"
    }

    fn request_body(
        &self,
        config: &AgentModelConfig,
        request: &ModelRequest,
    ) -> Result<Value, String> {
        Ok(responses_request(config, request))
    }

    fn parse_response(&self, body: &Value) -> Result<ModelResponse, String> {
        parse_responses_response(body).ok_or_else(|| "model response was unsupported".into())
    }

    fn parse_stream_event(&self, body: &Value) -> Result<Option<ModelStreamEvent>, String> {
        Ok(parse_responses_stream_event(body))
    }

    fn parse_stream_terminal_response(
        &self,
        body: &Value,
    ) -> Result<Option<ModelResponse>, String> {
        Ok(parse_responses_stream_terminal_response(body))
    }

    fn parse_usage(&self, body: &Value) -> Option<ModelTokenUsage> {
        parse_responses_usage(body)
    }
}

pub fn responses_request(config: &AgentModelConfig, request: &ModelRequest) -> Value {
    let instructions = responses_instructions_text(&request.messages);
    let input = request
        .response_input_items
        .as_ref()
        .map(|items| json!(items))
        .unwrap_or_else(|| responses_input_value(&request.messages));
    let mut payload = json!({
        "model": config.model_name,
        "input": input,
    });
    if let Some(instructions) = instructions {
        payload["instructions"] = json!(instructions);
    }
    if let Some(previous_response_id) = &request.previous_response_id {
        payload["previous_response_id"] = json!(previous_response_id);
    }
    if let Some(temperature) = config.temperature {
        payload["temperature"] = json!(temperature);
    }
    if let Some(max_tokens) = config.max_tokens {
        payload["max_output_tokens"] = json!(max_tokens);
    }
    if config.thinking_enabled {
        let mut reasoning = json!({ "summary": "concise" });
        if let Some(reasoning_effort) = config.reasoning_effort {
            reasoning["effort"] = json!(reasoning_effort.provider_value());
        }
        payload["reasoning"] = reasoning;
    }
    if !request.tools.is_empty() {
        payload["tools"] =
            crate::request_tool_schema_payload(AgentModelProtocol::Responses, request);
        payload["parallel_tool_calls"] = json!(true);
    } else {
        payload["tool_choice"] = json!("none");
    }
    payload
}

fn responses_input_value(messages: &[crate::AgentMessage]) -> Value {
    let input_messages = messages
        .iter()
        .filter(|message| message.role != "system")
        .collect::<Vec<_>>();
    if input_messages.len() == 1
        && input_messages[0].role == "user"
        && !input_messages[0].has_image_parts()
    {
        return json!(input_messages[0].text_content());
    }
    Value::Array(
        input_messages
            .into_iter()
            .flat_map(|message| {
                let function_items = message
                    .parts
                    .iter()
                    .filter_map(responses_function_call_item_json)
                    .collect::<Vec<_>>();
                let has_text = !message.plain_text_content().trim().is_empty();
                let mut items = Vec::new();
                if has_text || function_items.is_empty() {
                    items.push(responses_input_item_json(message));
                }
                items.extend(function_items);
                items
            })
            .collect::<Vec<_>>(),
    )
}

fn parse_responses_stream_event(body: &Value) -> Option<ModelStreamEvent> {
    let event_type = body.get("type").and_then(Value::as_str).unwrap_or_default();
    let event = match event_type {
        "response.output_text.delta" | "response.refusal.delta" => body
            .get("delta")
            .and_then(Value::as_str)
            .map(|text| ModelStreamEvent::text_delta(text.to_owned())),
        "response.reasoning_text.delta" | "response.reasoning.delta" => body
            .get("delta")
            .and_then(Value::as_str)
            .map(|text| ModelStreamEvent::reasoning_delta(text.to_owned())),
        "response.reasoning_summary_text.delta" => body
            .get("delta")
            .and_then(Value::as_str)
            .map(|text| ModelStreamEvent::reasoning_summary_delta(text.to_owned())),
        _ => None,
    }?;
    if event.delta.is_empty() {
        None
    } else {
        Some(event)
    }
}

fn parse_responses_stream_terminal_response(body: &Value) -> Option<ModelResponse> {
    match body.get("type").and_then(Value::as_str) {
        Some("response.completed") => body.get("response").and_then(parse_responses_response),
        _ => None,
    }
}

fn parse_responses_response(body: &Value) -> Option<ModelResponse> {
    let response_id = body.get("id").and_then(Value::as_str).map(str::to_owned);
    let token_usage = parse_responses_usage(body);
    if let Some(output) = body.get("output").and_then(Value::as_array) {
        let mut parts = Vec::new();
        for item in output {
            if let Some(tool_call) = parse_responses_tool_call(item) {
                parts.push(AgentMessagePart::ToolCall {
                    call_id: tool_call.call_id,
                    tool_name: tool_call.tool_name,
                    input: tool_call.input,
                    input_error: tool_call.input_error,
                });
                continue;
            }
            if item.get("type").and_then(Value::as_str) == Some("message")
                && let Some(text) = item.get("content").and_then(text_from_content)
            {
                parts.push(AgentMessagePart::Text { text });
                continue;
            }
            if let Some(summary) = parse_responses_reasoning_summary(item) {
                parts.push(AgentMessagePart::ReasoningSummary { text: summary });
            }
        }
        if parts.is_empty()
            && let Some(text) = output
                .iter()
                .find_map(|item| item.get("content").and_then(text_from_content))
        {
            parts.push(AgentMessagePart::Text { text });
        }
        let finish = if parts
            .iter()
            .any(|part| matches!(part, AgentMessagePart::ToolCall { .. }))
        {
            ModelFinish::ToolCalls
        } else {
            ModelFinish::Final
        };
        if !parts.is_empty() {
            return Some(ModelResponse::from_assistant(
                AgentMessage::new("assistant", parts),
                finish,
                token_usage,
                None,
                response_id,
            ));
        }
    }
    if let Some(text) = body.get("output_text").and_then(Value::as_str) {
        return Some(ModelResponse::from_assistant(
            AgentMessage::assistant(text),
            ModelFinish::Final,
            token_usage,
            None,
            response_id,
        ));
    }
    None
}

fn parse_responses_tool_call(item: &Value) -> Option<ToolCall> {
    match item.get("type").and_then(Value::as_str) {
        Some("function_call") | Some("custom_tool_call") => {
            let name = item.get("name")?.as_str()?.to_owned();
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .map(|value| value.to_owned());
            let (input, input_error) = parse_tool_call_arguments(item);
            Some(ToolCall {
                call_id,
                tool_name: name,
                input,
                input_error,
            })
        }
        _ => None,
    }
}

fn parse_tool_call_arguments(item: &Value) -> (Value, Option<String>) {
    let Some(raw_arguments) = item
        .get("arguments")
        .or_else(|| item.get("input"))
        .and_then(Value::as_str)
    else {
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

fn parse_responses_reasoning_summary(item: &Value) -> Option<String> {
    if item.get("type").and_then(Value::as_str) != Some("reasoning") {
        return None;
    }
    item.get("summary").and_then(text_from_content)
}
