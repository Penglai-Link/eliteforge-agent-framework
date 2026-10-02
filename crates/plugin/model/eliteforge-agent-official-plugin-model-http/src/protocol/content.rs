use serde_json::{Value, json};

use crate::{
    AgentMessage, AgentMessagePart, function_call_arguments_json, is_generated_tool_call_id,
};

pub(super) fn chat_message_json(message: &AgentMessage) -> Value {
    let content = if message.role == "tool" {
        message.text_content()
    } else {
        message.plain_text_content()
    };
    let mut payload = json!({ "role": provider_role(message) });
    payload["content"] = if message.role == "assistant"
        && content.is_empty()
        && message
            .parts
            .iter()
            .any(|part| matches!(part, AgentMessagePart::ToolCall { .. }))
    {
        Value::Null
    } else {
        chat_content_json(message, &content)
    };
    if message.role == "assistant" {
        let tool_calls = message
            .parts
            .iter()
            .filter_map(chat_tool_call_json)
            .collect::<Vec<_>>();
        if !tool_calls.is_empty() {
            payload["tool_calls"] = json!(tool_calls);
        }
    }
    if message.role == "tool"
        && let Some(call_id) = message.first_tool_call_id()
        && !is_generated_tool_call_id(call_id)
    {
        payload["role"] = json!("tool");
        payload["tool_call_id"] = json!(call_id);
        payload["content"] = json!(message.first_tool_output().unwrap_or_default());
    }
    payload
}

fn chat_content_json(message: &AgentMessage, content: &str) -> Value {
    let image_parts = message
        .parts
        .iter()
        .filter_map(image_data_url)
        .collect::<Vec<_>>();
    if message.role != "user" || image_parts.is_empty() {
        return json!(content);
    }
    let mut parts = Vec::new();
    if !content.trim().is_empty() {
        parts.push(json!({
            "type": "text",
            "text": content,
        }));
    }
    parts.extend(image_parts.into_iter().map(|url| {
        json!({
            "type": "image_url",
            "image_url": {
                "url": url,
            },
        })
    }));
    Value::Array(parts)
}

fn image_data_url(part: &AgentMessagePart) -> Option<String> {
    let AgentMessagePart::Image { mime_type, data } = part else {
        return None;
    };
    Some(
        if data.starts_with("http://") || data.starts_with("https://") || data.starts_with("data:")
        {
            data.clone()
        } else {
            format!("data:{mime_type};base64,{data}")
        },
    )
}

pub(super) fn responses_instructions_text(messages: &[AgentMessage]) -> Option<String> {
    let instructions = messages
        .iter()
        .filter(|message| matches!(message.role.as_str(), "system" | "developer"))
        .map(|message| message.plain_text_content())
        .map(|content| content.trim().to_string())
        .filter(|content| !content.is_empty())
        .collect::<Vec<_>>();
    if instructions.is_empty() {
        None
    } else {
        Some(instructions.join("\n\n"))
    }
}

fn provider_role(message: &AgentMessage) -> &str {
    match message.role.as_str() {
        // Tool observations without provider call ids cannot be serialized as
        // native Chat Completions tool results, so keep the legacy plain-text
        // fallback for older function_call-style responses.
        "tool" => "user",
        other => other,
    }
}

fn chat_tool_call_json(part: &AgentMessagePart) -> Option<Value> {
    let AgentMessagePart::ToolCall {
        call_id,
        tool_name,
        input,
        ..
    } = part
    else {
        return None;
    };
    Some(json!({
        "id": call_id.as_deref().unwrap_or(tool_name.as_str()),
        "type": "function",
        "function": {
            "name": tool_name,
            "arguments": function_call_arguments_json(input),
        },
    }))
}

pub(super) fn text_from_content(content: &Value) -> Option<String> {
    if let Some(text) = content.as_str() {
        return (!text.is_empty()).then(|| text.to_owned());
    }

    let text = content
        .as_array()?
        .iter()
        .filter_map(|item| {
            item.get("text")
                .or_else(|| item.get("content"))
                .and_then(Value::as_str)
        })
        .collect::<Vec<_>>()
        .join("");
    if text.is_empty() { None } else { Some(text) }
}
