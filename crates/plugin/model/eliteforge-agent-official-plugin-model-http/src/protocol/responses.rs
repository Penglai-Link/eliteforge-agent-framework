use serde_json::{Value, json};

use crate::{
    AgentMessage, AgentMessagePart, AgentModelConfig, AgentModelProtocol, ModelFinish,
    ModelOutputItem, ModelRequest, ModelResponse, ModelStreamEvent, ModelStreamEventKind, ModelTokenUsage, ToolCall,
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
    let instructions =
        responses_instructions_text(&request.messages, config.supports_developer_role);
    let input = match &request.response_input_items {
        Some(items) => {
            // Stateful callers supply only conversation deltas. Preserve system
            // messages in the delta without promoting them to developer authority.
            let mut input = request
                .messages
                .iter()
                .filter(|message| include_in_input(message, config.supports_developer_role))
                .filter(|message| message.role == "system")
                .map(responses_input_item_json)
                .collect::<Vec<_>>();
            input.extend(items.iter().cloned());
            Value::Array(input)
        }
        None => responses_input_value(&request.messages, config.supports_developer_role),
    };
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

fn include_in_input(message: &AgentMessage, supports_developer_role: bool) -> bool {
    !(supports_developer_role && message.role == "system" && message.instruction_origin.is_some())
}

fn responses_input_value(messages: &[AgentMessage], supports_developer_role: bool) -> Value {
    let input_messages = messages
        .iter()
        .filter(|message| include_in_input(message, supports_developer_role))
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
    if matches!(event_type, "response.output_item.added" | "response.output_item.done") {
        let item = body.get("item")?;
        let item_type = item.get("type").and_then(Value::as_str);
        if matches!(item_type, Some("function_call" | "custom_tool_call")) {
            return Some(ModelStreamEvent { kind: ModelStreamEventKind::ToolBoundary, delta: String::new(),
                output_index: body.get("output_index").and_then(Value::as_u64).and_then(|v| usize::try_from(v).ok()),
                item_id: item.get("id").and_then(Value::as_str).map(str::to_owned), phase: None });
        }
        if item_type != Some("message") { return None; }
        return Some(ModelStreamEvent {
            kind: if event_type.ends_with(".added") { ModelStreamEventKind::MessageStart } else { ModelStreamEventKind::MessageEnd },
            delta: if event_type.ends_with(".done") { item.get("content").and_then(text_from_content).unwrap_or_default() } else { String::new() },
            output_index: body.get("output_index").and_then(Value::as_u64).and_then(|v| usize::try_from(v).ok()),
            item_id: item.get("id").and_then(Value::as_str).map(str::to_owned),
            phase: item.get("phase").and_then(Value::as_str).map(str::to_owned),
        });
    }
    let mut event = match event_type {
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
    event.output_index = body.get("output_index").and_then(Value::as_u64).and_then(|v| usize::try_from(v).ok());
    event.item_id = body.get("item_id").and_then(Value::as_str).map(str::to_owned);
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
        let mut output_items = Vec::new();
        for (output_index, item) in output.iter().enumerate() {
            if let Some(tool_call) = parse_responses_tool_call(item) {
                output_items.push(ModelOutputItem::ToolCall { output_index, call: tool_call.clone() });
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
                output_items.push(ModelOutputItem::Message {
                    output_index,
                    item_id: item.get("id").and_then(Value::as_str).map(str::to_owned),
                    phase: item.get("phase").and_then(Value::as_str).map(str::to_owned),
                    text: text.clone(),
                });
                parts.push(AgentMessagePart::Text { text });
                continue;
            }
            if let Some(summary) = parse_responses_reasoning_summary(item) {
                output_items.push(ModelOutputItem::ReasoningSummary { output_index, text: summary.clone() });
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
        } else if output_items.iter().any(|item| matches!(item, ModelOutputItem::Message { phase: Some(phase), text, .. } if phase == "commentary" && !text.trim().is_empty()))
            && !output_items.iter().any(|item| matches!(item, ModelOutputItem::Message { phase, text, .. } if phase.as_deref() != Some("commentary") && !text.trim().is_empty()))
        {
            ModelFinish::Continue
        } else {
            ModelFinish::Final
        };
        if !parts.is_empty() {
            let mut response = ModelResponse::from_assistant(
                AgentMessage::new("assistant", parts),
                finish,
                token_usage,
                None,
                response_id,
            );
            let mut calls = response.tool_calls().into_iter();
            for item in &mut output_items {
                if let ModelOutputItem::ToolCall { call, .. } = item && let Some(normalized) = calls.next() {
                    *call = normalized;
                }
            }
            response.output_items = output_items;
            return Some(response);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_keeps_message_identity_phase_and_final_selection() {
        let start = parse_responses_stream_event(&json!({"type":"response.output_item.added","output_index":1,"item":{"id":"m1","type":"message","role":"assistant","phase":"commentary"}})).unwrap();
        assert_eq!(start.kind, ModelStreamEventKind::MessageStart);
        assert_eq!(start.output_index, Some(1));
        assert_eq!(start.phase.as_deref(), Some("commentary"));
        let delta = parse_responses_stream_event(&json!({"type":"response.output_text.delta","output_index":1,"item_id":"m1","delta":"Checking"})).unwrap();
        assert_eq!(delta.kind, ModelStreamEventKind::Text);
        assert_eq!(delta.output_index, Some(1));
        let done = parse_responses_stream_event(&json!({"type":"response.output_item.done","output_index":1,"item":{"id":"m1","type":"message","phase":"commentary","content":[{"type":"output_text","text":"Checking"}]}})).unwrap();
        assert_eq!(done.kind, ModelStreamEventKind::MessageEnd);
        assert_eq!(done.delta, "Checking");

        let response = parse_responses_response(&json!({"id":"r1","output":[
            {"id":"m1","type":"message","phase":"commentary","content":[{"type":"output_text","text":"Checking"}]},
            {"id":"m2","type":"message","phase":"final_answer","content":[{"type":"output_text","text":"Done"}]}
        ]})).unwrap();
        assert_eq!(response.output_text(), "Done");
        assert_eq!(response.output_items.len(), 2);
        assert!(matches!(&response.output_items[0], ModelOutputItem::Message { output_index: 0, phase: Some(phase), .. } if phase == "commentary"));
        assert!(matches!(&response.output_items[1], ModelOutputItem::Message { output_index: 1, phase: Some(phase), .. } if phase == "final_answer"));
        let commentary = parse_responses_response(&json!({"output":[{"type":"message","phase":"commentary","content":[{"type":"output_text","text":"Checking"}]}]})).unwrap();
        assert_eq!(commentary.finish, ModelFinish::Continue);
        assert!(commentary.output_text().is_empty());
    }

    #[test]
    fn assistant_phase_is_preserved_on_manual_replay() {
        let mut message = AgentMessage::assistant("Checking");
        message.phase = Some("commentary".into());
        let encoded = responses_input_item_json(&message);
        assert_eq!(encoded["phase"], "commentary");
        let mut user = AgentMessage::user("Question");
        user.phase = Some("commentary".into());
        assert!(responses_input_item_json(&user).get("phase").is_none());
    }
}
