use serde_json::Value;

use super::{AgentModelProtocol, EffectiveTool};

/// An immutable, protocol-specific view of the tools in one model request.
/// Source definitions are retained only to compare model-visible fields.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedToolSchema {
    protocol: AgentModelProtocol,
    source_tools: Vec<EffectiveTool>,
    payload: Value,
    estimated_tokens: u32,
    revision: u64,
}

impl PreparedToolSchema {
    pub fn new(
        protocol: AgentModelProtocol,
        source_tools: Vec<EffectiveTool>,
        payload: Value,
        revision: u64,
    ) -> Result<Self, String> {
        let estimated_tokens = if source_tools.is_empty() {
            0
        } else {
            let bytes = serde_json::to_vec(&payload).map_err(|error| error.to_string())?;
            u32::try_from(bytes.len().div_ceil(4)).unwrap_or(u32::MAX)
        };
        Ok(Self {
            protocol,
            source_tools,
            payload,
            estimated_tokens,
            revision,
        })
    }

    pub fn matches(&self, protocol: AgentModelProtocol, tools: &[EffectiveTool]) -> bool {
        self.protocol == protocol
            && self.source_tools.len() == tools.len()
            && self.source_tools.iter().zip(tools).all(|(left, right)| {
                left.tool_name == right.tool_name
                    && left.description == right.description
                    && left.input_schema == right.input_schema
            })
    }

    pub fn payload(&self) -> &Value {
        &self.payload
    }

    pub fn estimated_tokens(&self) -> u32 {
        self.estimated_tokens
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }
}
