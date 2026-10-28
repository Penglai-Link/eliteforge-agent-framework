use serde_json::{Value, json};

use crate::{AgentModelProtocol, EffectiveTool, ModelRequest};

/// Shared by measurement and protocol adapters so they use the same fields and wrappers.
pub fn tool_schema_payload(protocol: AgentModelProtocol, tools: &[EffectiveTool]) -> Value {
    Value::Array(
        tools
            .iter()
            .map(|tool| match protocol {
                AgentModelProtocol::ChatCompletions => json!({
                    "type": "function",
                    "function": {
                        "name": tool.tool_name,
                        "description": tool.description,
                        "parameters": tool.input_schema,
                    },
                }),
                AgentModelProtocol::Responses => json!({
                    "type": "function",
                    "name": tool.tool_name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                }),
                AgentModelProtocol::Embeddings => Value::Null,
                AgentModelProtocol::Anthropic => json!({
                    "name": tool.tool_name,
                    "description": tool.description,
                    "input_schema": tool.input_schema,
                }),
                AgentModelProtocol::Gemini => json!({
                    "name": tool.tool_name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                }),
            })
            .collect(),
    )
}

pub fn request_tool_schema_payload(protocol: AgentModelProtocol, request: &ModelRequest) -> Value {
    request
        .prepared_tool_schema
        .as_ref()
        .filter(|snapshot| snapshot.matches(protocol, &request.tools))
        .map(|snapshot| snapshot.payload().clone())
        .unwrap_or_else(|| tool_schema_payload(protocol, &request.tools))
}
