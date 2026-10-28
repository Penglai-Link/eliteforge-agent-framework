use serde::{Deserialize, Serialize};
use serde_json::Value;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AgentModelProtocol {
    ChatCompletions,
    Responses,
    Embeddings,
    Anthropic,
    Gemini,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectiveTool {
    pub tool_name: String,
    pub description: String,
    pub input_schema: Value,
}
