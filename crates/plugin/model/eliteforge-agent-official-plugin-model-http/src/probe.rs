use crate::*;
use async_trait::async_trait;
use eliteforge_agent_core as fw;
use std::sync::Arc;

/// Connectivity probes execute a public, budgeted JSON input. The host selects
/// its protocol payload; this adapter owns the one HTTP attempt and usage.
pub struct HttpProbeModel {
    pub config: AgentModelConfig,
    pub factory: Arc<dyn ModelTransportFactory>,
}
#[async_trait]
impl fw::ChatModel for HttpProbeModel {
    fn on_failure(&self, _: &fw::Extensions, error: &fw::ModelError) {
        self.factory.on_failure(&self.config, error);
    }
    async fn recover_authentication(
        &self,
        _: &fw::Extensions,
        error: &fw::ModelError,
    ) -> Result<bool, fw::ModelError> {
        self.factory
            .recover_authentication(&self.config, error)
            .await
    }
    async fn generate(
        &self,
        _: &str,
        request: fw::ChatRequest,
        _: &dyn fw::ChatStream,
    ) -> Result<fw::ChatResponse, fw::ModelError> {
        let [message] = request.messages.as_slice() else {
            return Err(fw::ModelError::Fatal(
                "probe requires one JSON input".into(),
            ));
        };
        let [fw::ContentBlock::Json { value }] = message.content.0.as_slice() else {
            return Err(fw::ModelError::Fatal("probe requires JSON input".into()));
        };
        let mut body = value.clone();
        if let Some(limit) = request.options.max_output_tokens {
            for name in ["max_tokens", "max_output_tokens"] {
                if body.get(name).is_some() {
                    body[name] = limit.into();
                }
            }
        }
        let response =
            self.factory
                .create(&self.config)
                .await?
                .probe(
                    self.config.api_path.as_deref().ok_or_else(|| {
                        fw::ModelError::Fatal("probe endpoint is required".into())
                    })?,
                    &body,
                )
                .await?;
        let usage = crate::operations::parse_usage(&response).unwrap_or_default();
        Ok(fw::ChatResponse {
            content: "verified".into(),
            usage,
            ..Default::default()
        })
    }
}

pub const MIN_PROBE_OUTPUT_TOKENS: u32 = 16;
#[derive(Clone, Copy)]
pub enum ModelProbeKind {
    ChatCompletionsText,
    ResponsesText,
    ResponsesImage,
    Embedding,
    Rerank,
}
impl ModelProbeKind {
    pub fn default_path(self) -> &'static str {
        match self {
            Self::ChatCompletionsText => "/chat/completions",
            Self::ResponsesText | Self::ResponsesImage => "/responses",
            Self::Embedding => "/embeddings",
            Self::Rerank => "/rerank",
        }
    }
    pub fn payload(self, model: &str) -> serde_json::Value {
        use serde_json::json;
        match self {
            Self::ChatCompletionsText => {
                json!({"model":model,"messages":[{"role":"user","content":"ping"}],"max_tokens":MIN_PROBE_OUTPUT_TOKENS})
            }
            Self::ResponsesText => {
                json!({"model":model,"input":"ping","max_output_tokens":MIN_PROBE_OUTPUT_TOKENS})
            }
            Self::ResponsesImage => {
                json!({"model":model,"input":[{"role":"user","content":[{"type":"input_text","text":"Generate a verification image."}]}],"max_output_tokens":MIN_PROBE_OUTPUT_TOKENS})
            }
            Self::Embedding => json!({"model":model,"input":"ping"}),
            Self::Rerank => json!({"model":model,"query":"ping","documents":["ping"]}),
        }
    }
}
