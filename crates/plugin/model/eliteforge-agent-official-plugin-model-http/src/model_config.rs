use serde::{Deserialize, Serialize};

use super::ReasoningEffort;

use super::AgentModelProtocol;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentModelConfig {
    pub model_code: String,
    pub protocol: AgentModelProtocol,
    pub base_url: String,
    pub api_path: Option<String>,
    pub api_key: String,
    pub model_name: String,
    #[serde(default)]
    pub thinking_enabled: bool,
    /// Opt in to sending Agent and plugin instructions as `developer` in Chat Completions.
    /// Other system messages remain `system` even when this is enabled.
    #[serde(default)]
    pub supports_developer_role: bool,
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    pub responses_stateful_enabled: bool,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    pub context_window_tokens: Option<u32>,
    pub top_p: Option<f64>,
    pub max_retries: Option<u32>,
}
