use crate::*;
use async_trait::async_trait;
use eliteforge_agent_core as fw;
use serde_json::{Value, json};
use std::sync::Arc;
pub struct HttpEmbeddingModel {
    pub config: AgentModelConfig,
    pub factory: Arc<dyn ModelTransportFactory>,
}
#[async_trait]
impl fw::EmbeddingModel for HttpEmbeddingModel {
    fn input_capacity(&self) -> Option<usize> {
        self.config.context_window_tokens.map(|v| v as usize)
    }
    fn input_token_estimate(&self, request: &fw::EmbeddingRequest) -> u64 {
        request.inputs.iter().map(fw::Content::estimated_tokens).fold(0usize, usize::saturating_add).max(1) as u64
    }

    fn on_failure(&self, error: &fw::ModelError) {
        self.factory.on_failure(&self.config, error);
    }
    async fn recover_authentication(&self, error: &fw::ModelError) -> Result<bool, fw::ModelError> {
        self.factory
            .recover_authentication(&self.config, error)
            .await
    }
    async fn embed(
        &self,
        _: &str,
        request: fw::EmbeddingRequest,
    ) -> Result<fw::EmbeddingResponse, fw::ModelError> {
        let body = self.factory.embedding_body(&self.config, &request)?;
        let response = self
            .factory
            .create(&self.config)
            .await?
            .json(
                self.config.api_path.as_deref().unwrap_or("/embeddings"),
                &body,
            )
            .await?;
        let data = response
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| fw::ModelError::Fatal("embedding response missing data".into()))?;
        let vectors = data
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let values = item
                    .get("embedding")
                    .and_then(Value::as_array)
                    .ok_or_else(|| fw::ModelError::Fatal("embedding vector missing".into()))?
                    .iter()
                    .map(|number| {
                        number.as_f64().map(|n| n as f32).ok_or_else(|| {
                            fw::ModelError::Fatal("embedding value not numeric".into())
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(fw::EmbeddingVector {
                    index: item
                        .get("index")
                        .and_then(Value::as_u64)
                        .map_or(index, |n| n as usize),
                    values,
                })
            })
            .collect::<Result<Vec<_>, fw::ModelError>>()?;
        Ok(fw::EmbeddingResponse {
            provider_model: response.get("model").and_then(Value::as_str)
                .map(str::trim).filter(|value| !value.is_empty()).map(str::to_owned),
            model: request
                .choice
                .model
                .ok_or_else(|| fw::ModelError::Fatal("embedding model missing".into()))?,
            vectors,
            input_tokens: response
                .pointer("/usage/prompt_tokens")
                .or_else(|| response.pointer("/usage/total_tokens"))
                .and_then(Value::as_u64),
        })
    }
}
pub struct HttpRerankModel {
    pub config: AgentModelConfig,
    pub factory: Arc<dyn ModelTransportFactory>,
    pub chat_fallback: bool,
    /// Host-managed rule for the Chat Completions fallback.
    pub chat_instruction: Option<String>,
}
#[async_trait]
impl fw::RerankModel for HttpRerankModel {
    fn input_capacity(&self) -> Option<usize> {
        self.config
            .context_window_tokens
            .map(|window| window.saturating_sub(self.output_token_reserve()) as usize)
    }

    fn on_failure(&self, error: &fw::ModelError) {
        self.factory.on_failure(&self.config, error);
    }
    async fn recover_authentication(&self, error: &fw::ModelError) -> Result<bool, fw::ModelError> {
        self.factory
            .recover_authentication(&self.config, error)
            .await
    }
    fn input_token_estimate(&self, request: &fw::RerankRequest) -> u64 {
        let body = if self.chat_fallback {
            let mut body = rerank_chat_body(request, self.chat_instruction.as_deref(), self.config.supports_developer_role);
            body["max_tokens"] = json!(self.config.max_tokens.unwrap_or(4096));
            body
        } else {
            rerank_native_body(&self.config, request)
        };
        fw::Content::from(body.to_string()).estimated_tokens() as u64
    }
    fn output_token_reserve(&self) -> u32 {
        if self.chat_fallback {
            self.config.max_tokens.unwrap_or(4096)
        } else {
            0
        }
    }
    async fn rerank(
        &self,
        model: &str,
        request: fw::RerankRequest,
    ) -> Result<fw::RerankResponse, fw::ModelError> {
        Ok(self.rerank_with_usage(model, request, None).await?.response)
    }
    async fn rerank_with_usage(
        &self,
        _: &str,
        request: fw::RerankRequest,
        output_limit: Option<u32>,
    ) -> Result<fw::RerankModelOutput, fw::ModelError> {
        let usage;
        let documents = request
            .documents
            .iter()
            .map(fw::Content::text)
            .collect::<Vec<_>>();
        let transport = self.factory.create(&self.config).await?;
        let results = if self.chat_fallback {
            let body = rerank_chat_body(&request, self.chat_instruction.as_deref(), self.config.supports_developer_role);
            let mut body = body;
            body["max_tokens"] =
                json!(output_limit.unwrap_or(self.config.max_tokens.unwrap_or(4096)));
            let response = transport.json("/chat/completions", &body).await?;
            usage = parse_usage(&response);
            let content = response
                .pointer("/choices/0/message/content")
                .and_then(Value::as_str)
                .ok_or_else(|| fw::ModelError::Fatal("rerank chat response missing text".into()))?;
            let indexes: Vec<usize> = serde_json::from_str(content).map_err(|_| {
                fw::ModelError::Fatal("rerank chat returned invalid indexes".into())
            })?;
            let mut seen = std::collections::HashSet::new();
            let mut indexes = indexes
                .into_iter()
                .filter(|i| *i < documents.len() && seen.insert(*i))
                .collect::<Vec<_>>();
            if indexes.is_empty() {
                return Err(fw::ModelError::Fatal(
                    "rerank returned no valid indexes".into(),
                ));
            }
            if let Some(top_n) = request.top_n {
                indexes.truncate(top_n);
            }
            let total = indexes.len().max(1) as f64;
            indexes
                .into_iter()
                .enumerate()
                .map(|(rank, index)| fw::RankedDocument {
                    index,
                    score: 1.0 - rank as f64 / total,
                })
                .collect()
        } else {
            let body = rerank_native_body(&self.config, &request);
            let response = transport
                .json(self.config.api_path.as_deref().unwrap_or("/rerank"), &body)
                .await?;
            usage = parse_usage(&response);
            response
                .get("results")
                .or_else(|| response.get("data"))
                .and_then(Value::as_array)
                .ok_or_else(|| fw::ModelError::Fatal("rerank response missing results".into()))?
                .iter()
                .map(|item| {
                    let index = item
                        .get("index")
                        .or_else(|| item.get("document_index"))
                        .or_else(|| item.pointer("/document/index"))
                        .and_then(Value::as_u64)
                        .ok_or_else(|| fw::ModelError::Fatal("rerank index missing".into()))?
                        as usize;
                    let score = item
                        .get("relevance_score")
                        .or_else(|| item.get("score"))
                        .and_then(Value::as_f64)
                        .ok_or_else(|| fw::ModelError::Fatal("rerank score missing".into()))?;
                    Ok(fw::RankedDocument { index, score })
                })
                .collect::<Result<Vec<_>, fw::ModelError>>()?
        };
        Ok(fw::RerankModelOutput {
            response: fw::RerankResponse {
                model: request
                    .choice
                    .model
                    .ok_or_else(|| fw::ModelError::Fatal("rerank model missing".into()))?,
                results,
            },
            usage,
        })
    }
}

pub(crate) fn parse_usage(response: &Value) -> Option<fw::Usage> {
    let usage = response.get("usage")?;
    let input = usage
        .get("prompt_tokens")
        .or_else(|| usage.get("input_tokens"))
        .and_then(Value::as_u64);
    let output = usage
        .get("completion_tokens")
        .or_else(|| usage.get("output_tokens"))
        .and_then(Value::as_u64);
    let total = usage.get("total_tokens").and_then(Value::as_u64);
    input.or(total).map(|input_tokens| fw::Usage {
        input_tokens,
        output_tokens: output.unwrap_or(0),
    })
}

const DEFAULT_CHAT_RERANK_PROMPT: &str = include_str!("../prompts/chat-rerank.txt");

fn rerank_chat_body(request: &fw::RerankRequest, instruction: Option<&str>, supports_developer_role: bool) -> Value {
    let numbered = request
        .documents
        .iter()
        .enumerate()
        .map(|(index, document)| format!("[{index}] {}", document.text()))
        .collect::<Vec<_>>()
        .join("\n\n");
    let instruction = instruction.unwrap_or(DEFAULT_CHAT_RERANK_PROMPT.trim());
    let role = if supports_developer_role { "developer" } else { "system" };
    json!({"messages":[{"role":role,"content":instruction},{"role":"user","content":format!("Query:\n{}\n\nCandidates:\n{numbered}",request.query.text())}],"temperature":0})
}

fn rerank_native_body(config: &AgentModelConfig, request: &fw::RerankRequest) -> Value {
    let mut body = json!({"model":config.model_name,"query":request.query.text(),"documents":request.documents.iter().map(fw::Content::text).collect::<Vec<_>>()});
    if let Some(top_n) = request.top_n {
        body["top_n"] = json!(top_n);
    }
    body
}

pub(crate) fn embedding_body(config: &AgentModelConfig, request: &fw::EmbeddingRequest) -> Value {
    let inputs = request
        .inputs
        .iter()
        .map(fw::Content::text)
        .collect::<Vec<_>>();
    let mut body = json!({"input":if inputs.len()==1 {json!(inputs[0])} else {json!(inputs)}});
    if !config.model_name.is_empty() {
        body["model"] = json!(config.model_name);
    }
    if let Some(dimensions) = request.dimensions {
        body["dimensions"] = json!(dimensions);
    }
    body
}
