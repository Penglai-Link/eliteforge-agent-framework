use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMessage {
    #[serde(default)]
    pub annotations: Vec<eliteforge_agent_core::MessageAnnotation>,
    pub role: String,
    pub parts: Vec<AgentMessagePart>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AgentMessagePart {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
    },
    ReasoningSummary {
        text: String,
    },
    ToolCall {
        call_id: Option<String>,
        tool_name: String,
        input: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_error: Option<String>,
    },
    ToolResult {
        call_id: Option<String>,
        tool_name: String,
        output: String,
        #[serde(default)]
        externalized: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Value>,
    },
    Context {
        kind: String,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Value>,
    },
    Image {
        mime_type: String,
        data: String,
    },
    CompactionCheckpoint {
        summary: String,
        recent: String,
        source_start_seq: i64,
        source_end_seq: i64,
    },
}

impl AgentMessage {
    pub fn new(role: impl Into<String>, parts: Vec<AgentMessagePart>) -> Self {
        Self {
            role: role.into(),
            annotations: Vec::new(),
            parts,
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self::text("system", content)
    }

    pub fn developer(content: impl Into<String>) -> Self {
        Self::text("developer", content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::text("user", content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::text("assistant", content)
    }

    pub fn tool(content: impl Into<String>) -> Self {
        let output = content.into();
        Self::new(
            "tool",
            vec![AgentMessagePart::ToolResult {
                call_id: None,
                tool_name: "unknown".into(),
                output,
                externalized: false,
                metadata: None,
            }],
        )
    }

    pub fn tool_with_call_id(
        content: impl Into<String>,
        tool_call_id: impl Into<String>,
        tool_output: impl Into<String>,
    ) -> Self {
        let output = tool_output.into();
        Self::new(
            "tool",
            vec![AgentMessagePart::ToolResult {
                call_id: Some(tool_call_id.into()),
                tool_name: tool_name_from_observation_text(&content.into()),
                output,
                externalized: false,
                metadata: None,
            }],
        )
    }

    pub fn tool_result(
        tool_name: impl Into<String>,
        call_id: Option<String>,
        output: impl Into<String>,
    ) -> Self {
        Self::new(
            "tool",
            vec![AgentMessagePart::ToolResult {
                call_id,
                tool_name: tool_name.into(),
                output: output.into(),
                externalized: false,
                metadata: None,
            }],
        )
    }

    pub fn assistant_tool_calls(
        calls: impl IntoIterator<Item = (Option<String>, String, Value)>,
    ) -> Self {
        Self::new(
            "assistant",
            calls
                .into_iter()
                .map(|(call_id, tool_name, input)| AgentMessagePart::ToolCall {
                    call_id,
                    tool_name,
                    input,
                    input_error: None,
                })
                .collect(),
        )
    }

    pub fn context_block(kind: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new(
            "user",
            vec![AgentMessagePart::Context {
                kind: kind.into(),
                text: text.into(),
                metadata: None,
            }],
        )
    }

    pub fn text_content(&self) -> String {
        self.parts
            .iter()
            .filter_map(AgentMessagePart::render_text)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    pub fn plain_text_content(&self) -> String {
        self.parts
            .iter()
            .filter_map(AgentMessagePart::render_plain_text)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    pub fn output_text_content(&self) -> String {
        let text = self
            .parts
            .iter()
            .filter_map(AgentMessagePart::render_output_text)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        if text.is_empty() {
            self.plain_text_content()
        } else {
            text
        }
    }

    pub fn first_tool_call_id(&self) -> Option<&str> {
        self.parts.iter().find_map(|part| match part {
            AgentMessagePart::ToolCall { call_id, .. }
            | AgentMessagePart::ToolResult { call_id, .. } => call_id.as_deref(),
            _ => None,
        })
    }

    pub fn first_tool_output(&self) -> Option<&str> {
        self.parts.iter().find_map(|part| match part {
            AgentMessagePart::ToolResult { output, .. } => Some(output.as_str()),
            _ => None,
        })
    }

    pub fn has_image_parts(&self) -> bool {
        self.parts
            .iter()
            .any(|part| matches!(part, AgentMessagePart::Image { .. }))
    }

    fn text(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self::new(
            role,
            vec![AgentMessagePart::Text {
                text: content.into(),
            }],
        )
    }
}

impl AgentMessagePart {
    pub fn render_plain_text(&self) -> Option<String> {
        match self {
            Self::Text { text } | Self::ReasoningSummary { text } | Self::Context { text, .. } => {
                Some(text.clone())
            }
            Self::Reasoning { .. }
            | Self::ToolCall { .. }
            | Self::ToolResult { .. }
            | Self::Image { .. } => None,
            Self::CompactionCheckpoint {
                summary, recent, ..
            } => Some(render_compaction_checkpoint(summary, recent)),
        }
    }

    pub fn render_output_text(&self) -> Option<String> {
        match self {
            Self::Text { text } => Some(text.clone()),
            Self::Image { .. } => None,
            _ => None,
        }
    }

    pub fn render_text(&self) -> Option<String> {
        match self {
            Self::Text { text } | Self::ReasoningSummary { text } | Self::Context { text, .. } => {
                Some(text.clone())
            }
            Self::Reasoning { .. } | Self::Image { .. } => None,
            Self::ToolCall {
                tool_name, input, ..
            } => Some(format!("tool_call:{tool_name}\nargs:\n{input}")),
            Self::ToolResult {
                tool_name,
                output,
                externalized,
                metadata,
                ..
            } => {
                if *externalized {
                    Some(format!(
                        "tool:{tool_name}\nresult externalized:\n{}",
                        metadata
                            .as_ref()
                            .map(Value::to_string)
                            .unwrap_or_else(|| "{}".into())
                    ))
                } else {
                    Some(format!("tool:{tool_name}\nresult:\n{output}"))
                }
            }
            Self::CompactionCheckpoint {
                summary, recent, ..
            } => Some(render_compaction_checkpoint(summary, recent)),
        }
    }
}

fn render_compaction_checkpoint(summary: &str, recent: &str) -> String {
    if recent.trim().is_empty() {
        return format!("Conversation checkpoint:\n{summary}");
    }
    format!("Conversation checkpoint:\n{summary}\n\nRecent retained context:\n{recent}")
}

fn tool_name_from_observation_text(content: &str) -> String {
    content
        .strip_prefix("tool:")
        .and_then(|rest| rest.lines().next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("unknown")
        .to_string()
}
