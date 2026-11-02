use std::collections::BTreeMap;

use serde_json::Value;

use crate::{
    AgentMessage, AgentMessagePart, AgentModelProtocol, ModelFinish, ModelResponse,
    ModelStreamEvent, ModelStreamEventKind, ModelTokenUsage, ToolCall,
};

#[derive(Debug, Clone, Default)]
pub(super) struct StreamAccumulator {
    text: String,
    reasoning: String,
    reasoning_summary: String,
    pub(super) token_usage: Option<ModelTokenUsage>,
    pub(super) terminal_response: Option<ModelResponse>,
    response_id: Option<String>,
    chat_tool_calls: BTreeMap<usize, ChatToolCallAccumulator>,
    response_message_ids: BTreeMap<usize, String>,
    pub(super) unindexed_response_text: bool,
    pub(super) indexed_response_message: bool,
}

#[derive(Debug, Clone, Default)]
struct ChatToolCallAccumulator {
    call_id: Option<String>,
    tool_name: String,
    arguments: String,
}

impl StreamAccumulator {
    /// Bind the provider's item IDs to output positions, including providers
    /// that omit an index from a later delta but retain the item ID.
    pub(super) fn associate_response_message(&mut self, event: &mut ModelStreamEvent) -> Result<(), String> {
        if !matches!(event.kind, ModelStreamEventKind::Text | ModelStreamEventKind::MessageStart | ModelStreamEventKind::MessageEnd) {
            return Ok(());
        }
        if event.output_index.is_none() {
            if let Some(id) = &event.item_id {
                event.output_index = self.response_message_ids.iter().find_map(|(index, known)| (known == id).then_some(*index));
            }
        }
        if let Some(index) = event.output_index {
            self.indexed_response_message = true;
            if let Some(id) = &event.item_id {
                if self.response_message_ids.get(&index).is_some_and(|known| known != id)
                    || self.response_message_ids.iter().any(|(known_index, known)| *known_index != index && known == id)
                {
                    return Err("model stream output item identity changed".into());
                }
                self.response_message_ids.insert(index, id.clone());
            }
        } else if event.kind == ModelStreamEventKind::Text && !event.delta.is_empty() {
            self.unindexed_response_text = true;
        }
        Ok(())
    }

    pub(super) fn validate_response_items(&self, response: &ModelResponse) -> Result<(), String> {
        for item in &response.output_items {
            if let crate::ModelOutputItem::Message { output_index, item_id: Some(id), .. } = item
                && (self.response_message_ids.get(output_index).is_some_and(|known| known != id)
                    || self.response_message_ids.iter().any(|(index, known)| index != output_index && known == id))
            {
                return Err("model terminal output item identity changed".into());
            }
        }
        Ok(())
    }

    pub(super) fn observe_delta(&mut self, delta: &ModelStreamEvent) {
        match delta.kind {
            ModelStreamEventKind::Text => self.text.push_str(&delta.delta),
            ModelStreamEventKind::Reasoning => self.reasoning.push_str(&delta.delta),
            ModelStreamEventKind::ReasoningSummary => self.reasoning_summary.push_str(&delta.delta),
            ModelStreamEventKind::MessageStart | ModelStreamEventKind::MessageEnd | ModelStreamEventKind::ToolBoundary | ModelStreamEventKind::ToolCallDelta => {}
        }
    }

    pub(super) fn observe_raw_event(&mut self, protocol: AgentModelProtocol, event: &Value) {
        if self.response_id.is_none() {
            self.response_id = event.get("id").and_then(Value::as_str).map(str::to_string);
        }
        if protocol != AgentModelProtocol::ChatCompletions {
            return;
        }
        let Some(delta) = event
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("delta"))
        else {
            return;
        };
        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for (fallback_index, call) in tool_calls.iter().enumerate() {
                let index = call
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|value| usize::try_from(value).ok())
                    .unwrap_or(fallback_index);
                self.merge_chat_tool_call_delta(index, call);
            }
        }
        if let Some(function_call) = delta.get("function_call") {
            self.merge_chat_function_call_delta(0, function_call);
        }
    }

    pub(super) fn into_response(self) -> Option<ModelResponse> {
        let tool_calls = self.chat_tool_calls();
        if !tool_calls.is_empty() {
            let mut parts = self.assistant_context_parts();
            parts.extend(
                tool_calls
                    .into_iter()
                    .map(|call| AgentMessagePart::ToolCall {
                        call_id: call.call_id,
                        tool_name: call.tool_name,
                        input: call.input,
                        input_error: call.input_error,
                    }),
            );
            return Some(ModelResponse::from_assistant(
                AgentMessage::new("assistant", parts),
                ModelFinish::ToolCalls,
                self.token_usage,
                None,
                self.response_id,
            ));
        }
        let parts = self.assistant_context_parts();
        if parts.is_empty() {
            return None;
        }
        Some(ModelResponse::from_assistant(
            AgentMessage::new("assistant", parts),
            ModelFinish::Final,
            self.token_usage,
            None,
            self.response_id,
        ))
    }

    fn merge_chat_tool_call_delta(&mut self, index: usize, call: &Value) {
        let entry = self.chat_tool_calls.entry(index).or_default();
        if entry.call_id.is_none() {
            entry.call_id = call
                .get("call_id")
                .or_else(|| call.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        if let Some(function) = call.get("function") {
            self.merge_chat_function_call_delta(index, function);
        }
    }

    fn merge_chat_function_call_delta(&mut self, index: usize, function: &Value) {
        let entry = self.chat_tool_calls.entry(index).or_default();
        if let Some(name) = function.get("name").and_then(Value::as_str) {
            entry.tool_name.push_str(name);
        }
        if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
            entry.arguments.push_str(arguments);
        }
    }

    fn assistant_context_parts(&self) -> Vec<AgentMessagePart> {
        let mut parts = Vec::new();
        if !self.text.is_empty() {
            parts.push(AgentMessagePart::Text {
                text: self.text.clone(),
            });
        }
        if !self.reasoning_summary.is_empty() {
            parts.push(AgentMessagePart::ReasoningSummary {
                text: self.reasoning_summary.clone(),
            });
        }
        if !self.reasoning.is_empty() {
            parts.push(AgentMessagePart::Reasoning {
                text: self.reasoning.clone(),
            });
        }
        parts
    }

    fn chat_tool_calls(&self) -> Vec<ToolCall> {
        self.chat_tool_calls
            .values()
            .filter_map(|call| {
                if call.tool_name.trim().is_empty() {
                    return None;
                }
                let (input, input_error) = parse_stream_tool_call_arguments(call.arguments.trim());
                Some(ToolCall {
                    call_id: call.call_id.clone(),
                    tool_name: call.tool_name.clone(),
                    input,
                    input_error,
                })
            })
            .collect()
    }
}

fn parse_stream_tool_call_arguments(raw_arguments: &str) -> (Value, Option<String>) {
    if raw_arguments.is_empty() {
        return (Value::Null, Some("function arguments are missing".into()));
    }
    match serde_json::from_str(raw_arguments) {
        Ok(input) => (input, None),
        Err(error) => (
            Value::Null,
            Some(format!("function arguments are invalid JSON: {error}")),
        ),
    }
}
