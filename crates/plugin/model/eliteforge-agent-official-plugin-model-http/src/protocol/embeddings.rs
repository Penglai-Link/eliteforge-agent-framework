use serde_json::{Value, json};

use crate::{
    AgentModelConfig, AgentModelProtocol, ModelRequest, ModelResponse, ModelStreamEvent,
    ModelTokenUsage,
};

use super::{registry::ModelProtocolAdapter, usage::parse_input_only_usage};

#[derive(Debug, Clone, Copy)]
pub struct EmbeddingsAdapter;

impl ModelProtocolAdapter for EmbeddingsAdapter {
    fn protocol(&self) -> AgentModelProtocol {
        AgentModelProtocol::Embeddings
    }

    fn default_path(&self) -> &'static str {
        "embeddings"
    }

    fn request_body(
        &self,
        _config: &AgentModelConfig,
        _request: &ModelRequest,
    ) -> Result<Value, String> {
        Err("embedding models cannot generate text".into())
    }

    fn parse_response(&self, _body: &Value) -> Result<ModelResponse, String> {
        Err("embedding models cannot generate text".into())
    }

    fn parse_stream_event(&self, _body: &Value) -> Result<Option<ModelStreamEvent>, String> {
        Err("embedding models cannot generate text".into())
    }

    fn parse_usage(&self, body: &Value) -> Option<ModelTokenUsage> {
        parse_input_only_usage(body)
    }
}

pub fn embeddings_request(config: &AgentModelConfig, input: &str) -> Value {
    json!({
        "model": config.model_name,
        "input": input,
    })
}
