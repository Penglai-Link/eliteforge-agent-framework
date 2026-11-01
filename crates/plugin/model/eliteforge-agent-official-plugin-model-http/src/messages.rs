//! Lossless host message boundary. Product context is data, never promoted to
//! system instructions; native tool call identity stays separate from invocation identity.
use crate::{AgentMessage, AgentMessagePart};
use eliteforge_agent_core::{
    Content, ContentBlock, MediaContent, MediaSource, Message, Role, ToolCall,
};

pub(crate) fn encode(message: &AgentMessage) -> Result<Message, String> {
    let role = match message.role.to_ascii_lowercase().as_str() {
        "system" => Role::System,
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        other => return Err(format!("unsupported conversation role: {other}")),
    };
    let mut result =
        Message::new(role, Content::default()).with_annotations(message.annotations.clone());
    result.instruction_origin = message.instruction_origin;
    result.phase = message.phase.clone();
    result.output_group_id = message.output_group_id.clone();
    for part in &message.parts {
        let block = match part {
            AgentMessagePart::Text { text } => ContentBlock::Text { text: text.clone() },
            AgentMessagePart::Image { mime_type, data } => ContentBlock::Media {
                media: MediaContent {
                    mime_type: mime_type.clone(),
                    source: MediaSource::Base64(data.clone()),
                    name: None,
                    // Charge the complete native payload plus image framing. This is
                    // a conservative transport estimate, not reported provider usage.
                    estimated_tokens: Some(
                        data.len()
                            .saturating_add(mime_type.len())
                            .saturating_add(2048),
                    ),
                },
            },
            AgentMessagePart::ToolCall {
                call_id,
                tool_name,
                input,
                ..
            } => {
                if role != Role::Assistant {
                    return Err("tool calls require assistant role".into());
                }
                let id = call_id
                    .as_ref()
                    .filter(|id| !id.is_empty())
                    .ok_or("tool call is missing provider identity")?;
                result
                    .tool_calls
                    .push(ToolCall::new(id, tool_name, input.clone()));
                ContentBlock::Json {
                    value: serde_json::json!({"wireMessagePart": part}),
                }
            }
            AgentMessagePart::ToolResult { call_id, .. } => {
                if role != Role::Tool || result.tool_call_id.is_some() {
                    return Err("one tool result per tool message is required".into());
                }
                result.tool_call_id = Some(
                    call_id
                        .as_ref()
                        .filter(|id| !id.is_empty())
                        .ok_or("tool result is missing provider identity")?
                        .clone(),
                );
                ContentBlock::Json {
                    value: serde_json::json!({"wireMessagePart": part}),
                }
            }
            _ => ContentBlock::Json {
                value: serde_json::json!({"wireMessagePart": part}),
            },
        };
        result.content.0.push(block);
    }
    if role == Role::Tool
        && let Some(metadata) = message.parts.iter().find_map(|part| match part {
            AgentMessagePart::ToolResult {
                metadata: Some(value),
                ..
            } => Some(value),
            _ => None,
        })
        && let Some(content) = metadata.get("frameworkNativeContent")
    {
        result.content = serde_json::from_value(content.clone())
            .map_err(|e| format!("invalid native tool content: {e}"))?;
        if let Some(value) = metadata.get("frameworkNativeResult") {
            result.tool_result = serde_json::from_value(value.clone())
                .map_err(|e| format!("invalid native tool result: {e}"))?;
        }
    }
    Ok(result)
}

pub(crate) fn decode(message: &Message) -> Result<AgentMessage, String> {
    let role = match message.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    // Reserved lossless records cannot create protocol identity. The SDK's
    // validated top-level fields remain authoritative across the native bridge.
    let mut declared_calls = std::collections::HashSet::new();
    if (!message.tool_calls.is_empty() && message.role != Role::Assistant)
        || message
            .tool_calls
            .iter()
            .any(|call| call.id.is_empty() || !declared_calls.insert(call.id.as_str()))
        || (message.tool_call_id.is_some() && message.role != Role::Tool)
        || (message.tool_result.is_some() && message.role != Role::Tool)
        || (message.role == Role::Tool && message.tool_call_id.as_deref().is_none_or(str::is_empty))
    {
        return Err("invalid top-level tool protocol identity".into());
    }
    let mut encoded_calls = std::collections::HashSet::new();
    let mut encoded_result = false;
    let mut parts = Vec::new();
    for block in &message.content.0 {
        match block {
            ContentBlock::Text { text } => {
                parts.push(AgentMessagePart::Text { text: text.clone() })
            }
            ContentBlock::Json { value } if value.get("wireMessagePart").is_some() => {
                let part: AgentMessagePart =
                    serde_json::from_value(value["wireMessagePart"].clone())
                        .map_err(|e| format!("invalid wire message part: {e}"))?;
                match &part {
                    AgentMessagePart::ToolCall {
                        call_id,
                        tool_name,
                        input,
                        ..
                    } => {
                        let id = call_id
                            .as_deref()
                            .filter(|id| !id.is_empty())
                            .ok_or("native tool call is missing protocol identity")?;
                        if message.role != Role::Assistant
                            || !encoded_calls.insert(id.to_owned())
                            || !message.tool_calls.iter().any(|call| {
                                call.id == id
                                    && call.call_name == *tool_name
                                    && call.arguments == *input
                            })
                        {
                            return Err(
                                "native tool call differs from top-level protocol identity".into(),
                            );
                        }
                    }
                    AgentMessagePart::ToolResult { call_id, .. } => {
                        if message.role != Role::Tool
                            || encoded_result
                            || call_id.as_deref() != message.tool_call_id.as_deref()
                        {
                            return Err(
                                "native tool result differs from top-level protocol identity"
                                    .into(),
                            );
                        }
                        encoded_result = true;
                    }
                    _ => {}
                }
                parts.push(part);
            }
            ContentBlock::Media { media } => {
                let data = match &media.source {
                    MediaSource::Base64(data) => data.clone(),
                    MediaSource::Url(url) => url.clone(),
                };
                parts.push(AgentMessagePart::Image {
                    mime_type: media.mime_type.clone(),
                    data,
                });
            }
            ContentBlock::Json { value } => parts.push(AgentMessagePart::Context {
                kind: "framework_json".into(),
                text: value.to_string(),
                metadata: Some(value.clone()),
            }),
            ContentBlock::Resource { resource } => parts.push(AgentMessagePart::Context {
                kind: "framework_resource".into(),
                text: serde_json::to_string(resource).map_err(|e| e.to_string())?,
                metadata: Some(serde_json::to_value(resource).map_err(|e| e.to_string())?),
            }),
        }
    }
    for call in &message.tool_calls {
        if !parts.iter().any(|part| matches!(part, AgentMessagePart::ToolCall { call_id: Some(id), .. } if id == &call.id)) {
            parts.push(AgentMessagePart::ToolCall { call_id: Some(call.id.clone()), tool_name: call.call_name.clone(), input: call.arguments.clone(), input_error: None });
        }
    }
    if message.role == Role::Tool
        && !parts
            .iter()
            .any(|part| matches!(part, AgentMessagePart::ToolResult { .. }))
    {
        parts = vec![AgentMessagePart::ToolResult {
            call_id: message.tool_call_id.clone(),
            tool_name: "framework".into(),
            output: message.content.protocol_text(),
            externalized: message
                .tool_result
                .as_ref()
                .is_some_and(|r| r.result_reference.is_some()),
            metadata: message
                .tool_result
                .as_ref()
                .map(serde_json::to_value)
                .transpose()
                .map_err(|e| e.to_string())?,
        }];
    }
    // A product codec wrapper already contains its exact DTO metadata. Only
    // genuine native content/result fields need the lossless tool sidecar.
    if message.role == Role::Tool
        && !(encoded_result && message.content.0.len() == 1 && message.tool_result.is_none())
    {
        for part in &mut parts {
            if let AgentMessagePart::ToolResult { metadata, .. } = part {
                let mut value = metadata.take().unwrap_or_else(|| serde_json::json!({}));
                if !value.is_object() {
                    value = serde_json::json!({"productMetadata":value});
                }
                value["frameworkNativeContent"] =
                    serde_json::to_value(&message.content).map_err(|e| e.to_string())?;
                value["frameworkNativeResult"] =
                    serde_json::to_value(&message.tool_result).map_err(|e| e.to_string())?;
                *metadata = Some(value);
            }
        }
    }
    let mut result = AgentMessage::new(role, parts);
    result.annotations = message.annotations.clone();
    result.instruction_origin = message.instruction_origin;
    result.phase = message.phase.clone();
    result.output_group_id = message.output_group_id.clone();
    Ok(result)
}

/// Legacy runs predate mandatory provider call IDs. Preserve their content while
/// pairing missing IDs deterministically; unmatched records remain labeled data.
pub(crate) fn encode_history(history: &[AgentMessage]) -> Result<Vec<Message>, String> {
    use std::collections::VecDeque;
    let mut output = Vec::new();
    let mut pending = VecDeque::<(String, String)>::new();
    let mut pending_group: Option<String> = None;
    let close_pending = |pending: &mut VecDeque<(String, String)>, output: &mut Vec<Message>| {
        for (name, id) in pending.drain(..) {
            output.push(Message::tool(id, format!("Legacy history has no recorded result for {name}; execution outcome is unknown.")));
        }
    };
    for (sequence, original) in history.iter().enumerate() {
        let mut message = original.clone();
        let same_response = message.role == "assistant"
            && pending_group.is_some()
            && pending_group == message.output_group_id;
        if message.role != "tool" && !same_response {
            close_pending(&mut pending, &mut output);
            pending_group = None;
        }
        for (part_index, part) in message.parts.iter_mut().enumerate() {
            if let AgentMessagePart::ToolCall {
                call_id, tool_name, ..
            } = part
            {
                let id =
                    call_id.get_or_insert_with(|| format!("wire-legacy-{sequence}-{part_index}"));
                if id.is_empty() {
                    *id = format!("wire-legacy-{sequence}-{part_index}");
                }
                if pending.is_empty() { pending_group = message.output_group_id.clone(); }
                pending.push_back((tool_name.clone(), id.clone()));
            }
            if let AgentMessagePart::ToolResult {
                call_id, tool_name, ..
            } = part
            {
                let found = pending.iter().position(|(name, id)| {
                    if let Some(call_id) = call_id.as_ref().filter(|id| !id.is_empty()) {
                        id == call_id
                    } else {
                        name == tool_name
                    }
                });
                if let Some(index) = found {
                    *call_id = Some(pending.remove(index).unwrap().1);
                    if pending.is_empty() { pending_group = None; }
                } else {
                    close_pending(&mut pending, &mut output);
                    output.push(Message::user(Content(vec![ContentBlock::Json {
                        value: serde_json::json!({"legacyUnpairedToolRecord":original}),
                    }])));
                    message.parts.clear();
                    break;
                }
            }
        }
        if !message.parts.is_empty() {
            output.push(encode(&message)?);
        }
    }
    close_pending(&mut pending, &mut output);
    Ok(output)
}
