use serde_json::{Value, json};

use crate::{
    AgentModelConfig, AgentModelProtocol, ModelRequest, ModelResponse, ModelStreamEvent,
    ModelTokenUsage,
};

use super::{ChatCompletionsAdapter, EmbeddingsAdapter, ResponsesAdapter};

pub trait ModelProtocolAdapter: Send + Sync {
    fn protocol(&self) -> AgentModelProtocol;
    fn default_path(&self) -> &'static str;
    fn request_body(
        &self,
        config: &AgentModelConfig,
        request: &ModelRequest,
    ) -> Result<Value, String>;
    fn stream_request_body(
        &self,
        config: &AgentModelConfig,
        request: &ModelRequest,
    ) -> Result<Value, String> {
        let mut body = self.request_body(config, request)?;
        body["stream"] = json!(true);
        Ok(body)
    }
    fn parse_response(&self, body: &Value) -> Result<ModelResponse, String>;
    fn parse_stream_event(&self, body: &Value) -> Result<Option<ModelStreamEvent>, String>;
    fn parse_stream_events(&self, body: &Value) -> Result<Vec<ModelStreamEvent>, String> {
        Ok(self.parse_stream_event(body)?.into_iter().collect())
    }
    fn parse_stream_terminal_response(
        &self,
        _body: &Value,
    ) -> Result<Option<ModelResponse>, String> {
        Ok(None)
    }
    fn parse_usage(&self, body: &Value) -> Option<ModelTokenUsage>;
}

pub fn adapter_for_protocol(
    protocol: AgentModelProtocol,
) -> Option<&'static dyn ModelProtocolAdapter> {
    match protocol {
        AgentModelProtocol::ChatCompletions => Some(&ChatCompletionsAdapter),
        AgentModelProtocol::Responses => Some(&ResponsesAdapter),
        AgentModelProtocol::Embeddings => Some(&EmbeddingsAdapter),
        // These protocols are implemented by the official genai adapter.
        AgentModelProtocol::Anthropic | AgentModelProtocol::Gemini => None,
    }
}
