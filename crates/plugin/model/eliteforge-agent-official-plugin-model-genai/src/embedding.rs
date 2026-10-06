use super::*;
use genai::embed::{EmbedOptions, EmbedRequest};

/// Text embeddings supported by the pinned genai SDK. No chat request conversion.
pub struct GenaiEmbeddingAdapter {
    connection: String,
    client: Client,
    adapter: AdapterKind,
    endpoint: Endpoint,
    auth: AuthData,
}
impl GenaiEmbeddingAdapter {
    pub fn from_chat(chat: &GenaiChatAdapter) -> Result<Self, ModelError> {
        let adapter = match chat.adapter {
            AdapterKind::OpenAI | AdapterKind::OpenAIResp => AdapterKind::OpenAI,
            AdapterKind::Gemini => AdapterKind::Gemini,
            AdapterKind::Cohere => AdapterKind::Cohere,
            _ => {
                return Err(ModelError::Fatal(
                    "this genai adapter does not provide registered embeddings".into(),
                ));
            }
        };
        Ok(Self {
            connection: chat.connection_id.clone(),
            client: chat.client.clone(),
            adapter,
            endpoint: chat.endpoint.clone(),
            auth: chat.auth.clone(),
        })
    }
    pub fn protocol(&self) -> &'static str {
        match self.adapter {
            AdapterKind::Gemini => "gemini/embed-content",
            AdapterKind::Cohere => "cohere/embed",
            _ => "openai/embeddings",
        }
    }
}
#[async_trait]
impl EmbeddingModel for GenaiEmbeddingAdapter {
    async fn embed(
        &self,
        model: &str,
        request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, ModelError> {
        let texts: Vec<_> = request
            .inputs
            .iter()
            .map(|content| {
                if content
                    .0
                    .iter()
                    .any(|b| !matches!(b, ContentBlock::Text { .. }))
                    || content.text().trim().is_empty()
                {
                    return Err(ModelError::Fatal(
                        "genai embeddings require nonempty text inputs".into(),
                    ));
                }
                Ok(content.text())
            })
            .collect::<Result<_, _>>()?;
        if texts.is_empty() || request.dimensions == Some(0) {
            return Err(ModelError::Fatal(
                "invalid embedding input or dimensions".into(),
            ));
        }
        let count = texts.len();
        let embedding_type = match (self.adapter, request.purpose) {
            (AdapterKind::Gemini, EmbeddingPurpose::Query) => Some("RETRIEVAL_QUERY"),
            (AdapterKind::Gemini, EmbeddingPurpose::Document) => Some("RETRIEVAL_DOCUMENT"),
            (AdapterKind::Gemini, EmbeddingPurpose::Similarity) => Some("SEMANTIC_SIMILARITY"),
            (AdapterKind::Cohere, EmbeddingPurpose::Query) => Some("search_query"),
            (AdapterKind::Cohere, EmbeddingPurpose::Document) => Some("search_document"),
            (AdapterKind::Cohere, EmbeddingPurpose::Similarity) => {
                return Err(ModelError::Fatal(
                    "Cohere similarity purpose requires an explicit supported task".into(),
                ));
            }
            _ => None,
        };
        let options = EmbedOptions {
            dimensions: request.dimensions,
            embedding_type: embedding_type.map(str::to_owned),
            capture_usage: Some(true),
            truncate: Some("NONE".into()),
            ..Default::default()
        };
        let target = ServiceTarget {
            model: ModelIden::new(self.adapter, model),
            endpoint: self.endpoint.clone(),
            auth: self.auth.clone(),
        };
        let response = self
            .client
            .exec_embed(target, EmbedRequest::new_batch(texts), Some(&options))
            .await
            .map_err(map_error)?;
        let mut response = EmbeddingResponse {
            provider_model: None,
            model: ModelRef::new(&self.connection, model),
            vectors: response
                .embeddings
                .into_iter()
                .map(|e| EmbeddingVector {
                    index: e.index,
                    values: e.vector,
                })
                .collect(),
            input_tokens: response
                .usage
                .prompt_tokens
                .and_then(|n| u64::try_from(n).ok()),
        };
        response.validate(count, request.dimensions)?;
        response.vectors.sort_by_key(|v| v.index);
        Ok(response)
    }
}
