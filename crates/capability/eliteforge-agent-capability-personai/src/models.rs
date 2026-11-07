//! Immutable model bindings capture one account context per catalog revision.
use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
#[cfg(test)]
use base64::Engine;
use crab_serverapi::{
    base::AuthorizedClientApi,
    error::ServerApiError,
    gateway::{ProtocolType, QualityType},
    multimedia::{self, TaskStatus},
};
use eliteforge_agent_core::{
    ChatModel, ChatRequest, ChatResponse, ChatStream, Content, ContentBlock, ContentDetectionModel,
    ContentDetectionRequest, ContentDetectionResponse, EmbeddingModel, EmbeddingRequest,
    EmbeddingResponse, EmbeddingVector, ImageGenerationModel, ImageGenerationRequest,
    ImageGenerationResponse, ImageJob, MediaContent, MediaSource, Modality, ModelCatalogEntry,
    ModelCatalogSnapshot, ModelConnection, ModelDescriptor, ModelError, ModelJobStatus,
    ModelOperation, ModelRef, ModelRegistration, OperationCapabilities, ProviderErrorCategory,
    ProviderModelError, RankedDocument, RealtimeModel, RerankModel, RerankModelOutput,
    RerankRequest, RerankResponse, ResourceReference, Role, SpeechRecognitionModel,
    SpeechRecognitionRequest, SpeechRecognitionResponse, ToolCall, Usage, VideoGenerationModel,
    VideoGenerationRequest, VideoJob,
};
use eliteforge_agent_framework::ModelRuntimeUpdate;
use reqwest::Method;
use serde_json::{Value, json};

use crate::realtime::PersonAiRealtimeAdapter;
use crate::sync::{DiscoveredChatModel, DiscoveredMediaModel, PersonAiDiscoverySnapshot};
use crate::{AccountContext, PersonAiConfig, PersonAiContextProvider, PersonAiServices};

/// Factory for immutable per-account bindings. It is also exposed as a typed service.
pub struct PersonAiModels {
    connection_id: String,
    services: Arc<PersonAiServices>,
    context: Arc<dyn PersonAiContextProvider>,
    api: Arc<AuthorizedClientApi>,
    pub(crate) refresh: Arc<tokio::sync::Notify>,
}

impl PersonAiModels {
    pub fn new(
        connection_id: String,
        server_base_url: &str,
        services: Arc<PersonAiServices>,
        context: Arc<dyn PersonAiContextProvider>,
    ) -> crab_serverapi::error::Result<Self> {
        Ok(Self {
            connection_id,
            services,
            context,
            api: Arc::new(AuthorizedClientApi::new(server_base_url)?),
            refresh: Arc::new(tokio::sync::Notify::new()),
        })
    }

    pub fn bind(&self, account: Arc<AccountContext>) -> Arc<BoundPersonAiModels> {
        Arc::new(BoundPersonAiModels {
            connection_id: self.connection_id.clone(),
            services: self.services.clone(),
            context: self.context.clone(),
            api: self.api.clone(),
            account,
            refresh: self.refresh.clone(),
        })
    }

    pub fn bind_current(&self) -> Result<Arc<BoundPersonAiModels>, ModelError> {
        let account = self
            .context
            .current()
            .ok_or_else(|| ModelError::Fatal("PersonAI is not signed in".into()))?;
        Ok(self.bind(account))
    }

    pub fn runtime_update(
        &self,
        account: Arc<AccountContext>,
        snapshot: &PersonAiDiscoverySnapshot,
        config: &PersonAiConfig,
    ) -> Result<ModelRuntimeUpdate, ModelError> {
        let binding = self.bind(account);
        let mut descriptors: Vec<ModelDescriptor> =
            snapshot.chat.iter().map(chat_descriptor).collect();
        for id in &config.responses_only_models {
            if !descriptors.iter().any(|existing| existing.id == *id) {
                descriptors.push(ModelDescriptor::new(id));
            }
        }
        let mut connections = Vec::new();
        if let Some(first) = descriptors.first() {
            let protocol = chat_protocol(snapshot.chat.iter().find(|model| model.id == first.id));
            let quality = chat_quality(snapshot.chat.iter().find(|model| model.id == first.id));
            let default: Arc<dyn ChatModel> = Arc::new(PersonAiChatAdapter {
                binding: binding.clone(),
                protocol,
                quality,
            });
            let mut connection =
                ModelConnection::new(&config.connection_id, default, descriptors.clone())
                    .with_protocol(protocol_name(protocol));
            for descriptor in &descriptors {
                let protocol =
                    chat_protocol(snapshot.chat.iter().find(|model| model.id == descriptor.id));
                let adapter: Arc<dyn ChatModel> = Arc::new(PersonAiChatAdapter {
                    binding: binding.clone(),
                    protocol,
                    quality: chat_quality(
                        snapshot.chat.iter().find(|model| model.id == descriptor.id),
                    ),
                });
                connection = connection
                    .bind(&descriptor.id, adapter, protocol_name(protocol))
                    .map_err(|error| ModelError::Fatal(error.to_string()))?;
            }
            connections.push(connection);
        }
        let mut update = ModelRuntimeUpdate::new(
            &snapshot.account_scope,
            snapshot.account_revision,
            connections,
        );
        for id in &config.embedding_models {
            if config.multimodal_embedding_models.contains(id) {
                continue;
            }
            let implementation: Arc<dyn EmbeddingModel> = binding.clone();
            let registration =
                ModelRegistration::<dyn EmbeddingModel>::new(binding.model_ref(id), implementation)
                    .protocol("personai.embeddings");
            update.embedding.push(registration);
        }
        for id in &config.multimodal_embedding_models {
            let implementation: Arc<dyn EmbeddingModel> = binding.clone();
            update.embedding.push(
                ModelRegistration::<dyn EmbeddingModel>::new(binding.model_ref(id), implementation)
                    .protocol("personai.embeddings")
                    .capabilities(OperationCapabilities {
                        input: vec![Modality::Text, Modality::Image],
                        ..Default::default()
                    }),
            );
        }
        for (media_type, models) in &snapshot.media {
            for model in models {
                match *media_type {
                    multimedia::MediaType::EMBEDDING => {
                        // Configured host bindings take precedence; discovered capability comes only from tags.
                        if config.embedding_models.contains(&model.model_code)
                            || config
                                .multimodal_embedding_models
                                .contains(&model.model_code)
                        {
                            continue;
                        }
                        let input = embedding_modalities(&model.tags);
                        if input.is_empty() {
                            continue;
                        }
                        let implementation: Arc<dyn EmbeddingModel> = binding.clone();
                        update.embedding.push(
                            ModelRegistration::<dyn EmbeddingModel>::new(
                                binding.model_ref(&model.id),
                                implementation,
                            )
                            .protocol("personai.embeddings")
                            .capabilities(OperationCapabilities {
                                input,
                                ..Default::default()
                            }),
                        );
                    }
                    PERSONAI_RERANK_MEDIA_TYPE => {
                        let implementation: Arc<dyn RerankModel> = binding.clone();
                        update.rerank.push(
                            ModelRegistration::<dyn RerankModel>::new(
                                binding.model_ref(&model.id),
                                implementation,
                            )
                            .protocol("personai.rerank"),
                        );
                    }
                    multimedia::MediaType::IMAGE => {
                        let implementation: Arc<dyn ImageGenerationModel> = binding.clone();
                        update.image.push(
                            ModelRegistration::<dyn ImageGenerationModel>::new(
                                binding.model_ref(&model.id),
                                implementation,
                            )
                            .protocol("personai.multimedia"),
                        );
                    }
                    multimedia::MediaType::VIDEO => {
                        let implementation: Arc<dyn VideoGenerationModel> = binding.clone();
                        update.video.push(
                            ModelRegistration::<dyn VideoGenerationModel>::new(
                                binding.model_ref(&model.id),
                                implementation,
                            )
                            .protocol("personai.multimedia"),
                        );
                    }
                    multimedia::MediaType::AIGC_TEXT_DETECTION
                    | multimedia::MediaType::AIGC_IMAGE_DETECTION => {
                        let implementation: Arc<dyn ContentDetectionModel> = binding.clone();
                        let input = if *media_type == multimedia::MediaType::AIGC_TEXT_DETECTION {
                            vec![Modality::Text]
                        } else {
                            vec![Modality::Image]
                        };
                        update.detection.push(
                            ModelRegistration::<dyn ContentDetectionModel>::new(
                                binding.model_ref(&model.id),
                                implementation,
                            )
                            .protocol("personai.aigc-detection")
                            .capabilities(OperationCapabilities {
                                input,
                                output: vec![Modality::Text],
                                streaming: false,
                                cancellation: false,
                            }),
                        );
                    }
                    _ => {}
                }
            }
        }
        if snapshot
            .media
            .get(&multimedia::MediaType::SPEECH)
            .is_some_and(|models| !models.is_empty())
        {
            let implementation: Arc<dyn SpeechRecognitionModel> = binding.clone();
            update.recognition.push(
                ModelRegistration::<dyn SpeechRecognitionModel>::new(
                    binding.model_ref("speech-auto"),
                    implementation,
                )
                .protocol("personai.audio-transcriptions"),
            );
            let realtime: Arc<dyn RealtimeModel> = Arc::new(PersonAiRealtimeAdapter {
                binding: binding.clone(),
                server_base_url: config.server_base_url.clone(),
            });
            update.realtime.push(
                ModelRegistration::<dyn RealtimeModel>::new(
                    binding.model_ref("speech-auto"),
                    realtime,
                )
                .protocol("personai.audio-transcriptions.ws")
                .capabilities(OperationCapabilities {
                    input: vec![Modality::Audio],
                    output: vec![Modality::Text],
                    streaming: true,
                    cancellation: true,
                }),
            );
        }
        Ok(update)
    }

    pub fn catalog_snapshot(
        &self,
        snapshot: &PersonAiDiscoverySnapshot,
        config: &PersonAiConfig,
    ) -> ModelCatalogSnapshot {
        let mut entries = Vec::new();
        for model in &snapshot.chat {
            let protocol = chat_protocol(Some(model));
            entries.push(ModelCatalogEntry {
                operation: ModelOperation::Chat,
                model: ModelRef::new(&config.connection_id, &model.id),
                protocol: protocol_name(protocol).into(),
                capabilities: OperationCapabilities::default(),
                descriptor: Some(chat_descriptor(model)),
                metadata: BTreeMap::from([(
                    "personai.chat".into(),
                    serde_json::to_value(model).unwrap_or(Value::Null),
                )]),
            });
        }
        for (media_type, models) in &snapshot.media {
            let Some(operation) = media_operation(*media_type) else {
                continue;
            };
            for model in models {
                if operation == ModelOperation::Embedding
                    && embedding_modalities(&model.tags).is_empty()
                {
                    continue;
                }
                entries.push(ModelCatalogEntry {
                    operation,
                    model: ModelRef::new(&config.connection_id, &model.id),
                    protocol: if operation == ModelOperation::Rerank {
                        "personai.rerank"
                    } else if operation == ModelOperation::Embedding {
                        "personai.embeddings"
                    } else {
                        "personai.multimedia"
                    }
                    .into(),
                    capabilities: if operation == ModelOperation::Embedding {
                        OperationCapabilities {
                            input: embedding_modalities(&model.tags),
                            ..Default::default()
                        }
                    } else {
                        OperationCapabilities::default()
                    },
                    descriptor: None,
                    metadata: BTreeMap::from([(
                        "personai.media".into(),
                        serde_json::to_value(model).unwrap_or(Value::Null),
                    )]),
                });
            }
        }
        ModelCatalogSnapshot {
            account_scope: snapshot.account_scope.clone(),
            revision: snapshot.account_revision,
            synced_at_unix_ms: snapshot.synced_at_unix_ms,
            entries,
        }
    }
}

pub fn restore_discovery(cached: &ModelCatalogSnapshot) -> Option<PersonAiDiscoverySnapshot> {
    let mut restored = PersonAiDiscoverySnapshot {
        account_scope: cached.account_scope.clone(),
        account_revision: cached.revision,
        synced_at_unix_ms: cached.synced_at_unix_ms,
        ..Default::default()
    };
    for entry in &cached.entries {
        if let Some(chat) = entry.metadata.get("personai.chat") {
            restored
                .chat
                .push(serde_json::from_value(chat.clone()).ok()?);
        }
        if let Some(media) = entry.metadata.get("personai.media") {
            let model: DiscoveredMediaModel = serde_json::from_value(media.clone()).ok()?;
            restored
                .media
                .entry(model.media_type)
                .or_default()
                .push(model);
        }
    }
    Some(restored)
}

pub(crate) const PERSONAI_RERANK_MEDIA_TYPE: i32 = multimedia::MediaType::RERANK;

fn embedding_modalities(tags: &[String]) -> Vec<Modality> {
    if tags.iter().any(|tag| tag == "multimodal_embedding") {
        vec![Modality::Text, Modality::Image]
    } else if tags.iter().any(|tag| tag == "text_embedding") {
        vec![Modality::Text]
    } else {
        vec![]
    }
}

fn media_operation(media_type: i32) -> Option<ModelOperation> {
    match media_type {
        multimedia::MediaType::EMBEDDING => Some(ModelOperation::Embedding),
        PERSONAI_RERANK_MEDIA_TYPE => Some(ModelOperation::Rerank),
        multimedia::MediaType::IMAGE => Some(ModelOperation::ImageGeneration),
        multimedia::MediaType::VIDEO => Some(ModelOperation::VideoGeneration),
        multimedia::MediaType::SPEECH => Some(ModelOperation::SpeechRecognition),
        multimedia::MediaType::AIGC_TEXT_DETECTION
        | multimedia::MediaType::AIGC_IMAGE_DETECTION => Some(ModelOperation::ContentDetection),
        _ => None,
    }
}

fn chat_descriptor(model: &DiscoveredChatModel) -> ModelDescriptor {
    let mut descriptor = ModelDescriptor::new(&model.id);
    descriptor
        .metadata
        .insert(crate::PERSONAI_PROVIDER.into(), true.into());
    descriptor.context_window_tokens = model
        .context_length
        .and_then(|count| usize::try_from(count).ok());
    if model.image_understanding {
        descriptor.input_modalities.push(Modality::Image);
    }
    descriptor.metadata.insert(
        "personai.video_understanding".into(),
        json!(model.video_understanding),
    );
    descriptor
        .metadata
        .insert("personai.quality_tiers".into(), json!(model.quality_tiers));
    descriptor
        .metadata
        .insert("personai.model_code".into(), json!(model.model_code));
    descriptor
        .metadata
        .insert("personai.provider_code".into(), json!(model.provider_code));
    descriptor
        .metadata
        .insert("personai.model_name".into(), json!(model.model_name));
    descriptor
}

fn chat_protocol(model: Option<&DiscoveredChatModel>) -> ProtocolType {
    if model.is_none_or(|model| {
        !model
            .protocols
            .iter()
            .any(|protocol| protocol == "chat_completions")
    }) {
        ProtocolType::Responses
    } else {
        ProtocolType::ChatCompletions
    }
}

fn chat_quality(model: Option<&DiscoveredChatModel>) -> QualityType {
    match model {
        Some(model) if model.quality_tiers.contains("low") => QualityType::Low,
        Some(model) if model.quality_tiers.contains("middle") => QualityType::Middle,
        _ => QualityType::High,
    }
}

fn protocol_name(protocol: ProtocolType) -> &'static str {
    match protocol {
        ProtocolType::ChatCompletions => "personai.chat-completions",
        ProtocolType::Responses => "personai.responses",
    }
}

pub struct BoundPersonAiModels {
    connection_id: String,
    services: Arc<PersonAiServices>,
    context: Arc<dyn PersonAiContextProvider>,
    api: Arc<AuthorizedClientApi>,
    account: Arc<AccountContext>,
    refresh: Arc<tokio::sync::Notify>,
}

impl BoundPersonAiModels {
    fn ensure_open(&self) -> Result<(), ModelError> {
        if self.context.is_closed() {
            return Err(ModelError::Provider(Box::new(ProviderModelError {
                authorization_attempt: None,
                provider: "personai".into(),
                category: ProviderErrorCategory::AccountChanged,
                http_status: None,
                business_code: None,
                error_type: None,
                model: None,
                message: "PersonAI connection is closed".into(),
            })));
        }
        Ok(())
    }
    fn provider_error(&self, error: ServerApiError, model: &str) -> ModelError {
        let error = map_error(error).with_model(self.model_ref(model));
        if error.is_unavailable() {
            self.refresh.notify_one();
        }
        error
    }
    pub(crate) async fn search(
        &self,
        query: &str,
        count: Option<i32>,
    ) -> Result<crab_serverapi::gateway::SearchResponse, ModelError> {
        self.ensure_current()?;
        self.services
            .search
            .search(&self.account.authorization, query, count)
            .await
            .map_err(map_error)
    }

    pub fn account_scope(&self) -> &str {
        &self.account.account_scope
    }
    pub fn account_revision(&self) -> u64 {
        self.account.revision
    }
    pub fn model_ref(&self, id: &str) -> ModelRef {
        ModelRef::new(&self.connection_id, id)
    }

    pub(crate) fn ensure_current(&self) -> Result<(), ModelError> {
        if self
            .context
            .current()
            .is_some_and(|current| Arc::ptr_eq(&current, &self.account))
        {
            Ok(())
        } else {
            Err(ModelError::Provider(Box::new(ProviderModelError {
                authorization_attempt: None,
                provider: "personai".into(),
                category: ProviderErrorCategory::AccountChanged,
                http_status: None,
                business_code: None,
                error_type: None,
                model: None,
                message: "PersonAI account context changed or connection closed".into(),
            })))
        }
    }

    pub(crate) fn authorization(&self) -> &crab_serverapi::base::AuthorizedClientContext {
        &self.account.authorization
    }

    pub async fn chat_with_protocol(
        &self,
        model_id: &str,
        protocol: ProtocolType,
        quality: QualityType,
        request: ChatRequest,
        stream: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        self.ensure_current()?;
        let body = match protocol {
            ProtocolType::ChatCompletions => chat_completions_request(model_id, &request)?,
            ProtocolType::Responses => responses_request(model_id, &request)?,
        };
        let raw =
            serde_json::to_string(&body).map_err(|error| ModelError::Fatal(error.to_string()))?;
        let context = &self.account.authorization;
        let response = match protocol {
            ProtocolType::ChatCompletions => {
                self.services
                    .chat
                    .create(context, &raw, Some(quality), None)
                    .await
            }
            ProtocolType::Responses => self.services.responses.create(context, &raw).await,
        }
        .map_err(|error| self.provider_error(error, model_id))?;
        let parsed = match protocol {
            ProtocolType::ChatCompletions => parse_chat_response(response)?,
            ProtocolType::Responses => parse_responses_response(response)?,
        };
        if !parsed.content.is_empty() {
            stream.text_delta(&parsed.content.text()).await;
        }
        Ok(parsed)
    }

    pub async fn stream_chat_with_protocol(
        &self,
        model_id: &str,
        protocol: ProtocolType,
        quality: QualityType,
        request: ChatRequest,
        stream: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        self.ensure_current()?;
        let body = match protocol {
            ProtocolType::ChatCompletions => {
                let mut body = chat_completions_request(model_id, &request)?;
                // The SDK adds `stream`, but usage must be requested explicitly.
                body["stream_options"] = json!({ "include_usage": true });
                body
            }
            ProtocolType::Responses => responses_request(model_id, &request)?,
        };
        let raw =
            serde_json::to_string(&body).map_err(|error| ModelError::Fatal(error.to_string()))?;
        let context = &self.account.authorization;
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<String>();
        let future = async {
            match protocol {
                ProtocolType::ChatCompletions => {
                    self.services
                        .chat
                        .create_stream(context, &raw, quality, None, move |data| {
                            let _ = sender.send(data.to_string());
                            Ok(())
                        })
                        .await
                }
                ProtocolType::Responses => {
                    self.services
                        .responses
                        .create_stream(context, &raw, move |data| {
                            let _ = sender.send(data.to_string());
                            Ok(())
                        })
                        .await
                }
            }
        };
        tokio::pin!(future);
        let mut completed = false;
        let mut collected = StreamAccumulator::default();
        loop {
            tokio::select! {
                result = &mut future, if !completed => {
                    result.map_err(|error| self.provider_error(error, model_id))?;
                    completed = true;
                }
                chunk = receiver.recv() => match chunk {
                    Some(chunk) => collected.accept(protocol, &chunk, stream).await?,
                    None if completed => break,
                    None => return Err(ModelError::Transient("PersonAI model stream ended before request completion".into())),
                },
            }
        }
        collected.finish()
    }
}

/// One protocol per adapter value ensures catalog bindings cannot silently change wire format.
pub struct PersonAiChatAdapter {
    pub binding: Arc<BoundPersonAiModels>,
    pub protocol: ProtocolType,
    pub quality: QualityType,
}

#[async_trait]
impl ChatModel for PersonAiChatAdapter {
    async fn generate(
        &self,
        model_id: &str,
        request: ChatRequest,
        stream: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        self.binding
            .stream_chat_with_protocol(model_id, self.protocol, self.quality, request, stream)
            .await
    }
}

#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
struct StreamAccumulator {
    text: String,
    calls: BTreeMap<String, PartialToolCall>,
    usage: Usage,
}

impl StreamAccumulator {
    async fn accept(
        &mut self,
        protocol: ProtocolType,
        data: &str,
        stream: &dyn ChatStream,
    ) -> Result<(), ModelError> {
        if data == "[DONE]" || data.trim().is_empty() {
            return Ok(());
        }
        let event: Value = serde_json::from_str(data)
            .map_err(|error| ModelError::Fatal(format!("invalid PersonAI SSE payload: {error}")))?;
        if let Some(error) = event.get("error") {
            return Err(ModelError::Fatal(format!("PersonAI model stream: {error}")));
        }
        match protocol {
            ProtocolType::ChatCompletions => {
                let delta = &event["choices"][0]["delta"];
                if let Some(value) = delta["content"].as_str() {
                    self.text.push_str(value);
                    stream.text_delta(value).await;
                }
                if let Some(value) = delta["reasoning_content"].as_str() {
                    stream.reasoning_delta(value).await;
                }
                if let Some(calls) = delta["tool_calls"].as_array() {
                    for call in calls {
                        let key = call["index"].as_u64().unwrap_or(0).to_string();
                        let partial = self.calls.entry(key).or_default();
                        if let Some(id) = call["id"].as_str() {
                            partial.id.push_str(id);
                        }
                        if let Some(name) = call["function"]["name"].as_str() {
                            partial.name.push_str(name);
                        }
                        if let Some(args) = call["function"]["arguments"].as_str() {
                            partial.arguments.push_str(args);
                        }
                    }
                }
                self.usage.input_tokens = event["usage"]["prompt_tokens"]
                    .as_u64()
                    .unwrap_or(self.usage.input_tokens);
                self.usage.output_tokens = event["usage"]["completion_tokens"]
                    .as_u64()
                    .unwrap_or(self.usage.output_tokens);
            }
            ProtocolType::Responses => match event["type"].as_str().unwrap_or_default() {
                "response.output_text.delta" => {
                    if let Some(value) = event["delta"].as_str() {
                        self.text.push_str(value);
                        stream.text_delta(value).await;
                    }
                }
                "response.reasoning_text.delta" => {
                    if let Some(value) = event["delta"].as_str() {
                        stream.reasoning_delta(value).await;
                    }
                }
                "response.output_item.added" => {
                    let item = &event["item"];
                    if item["type"] == "function_call" {
                        let key = item["id"].as_str().unwrap_or_default();
                        self.calls.insert(
                            key.into(),
                            PartialToolCall {
                                id: item["call_id"].as_str().unwrap_or_default().into(),
                                name: item["name"].as_str().unwrap_or_default().into(),
                                arguments: String::new(),
                            },
                        );
                    }
                }
                "response.function_call_arguments.delta" => {
                    let key = event["item_id"].as_str().unwrap_or_default();
                    if let Some(partial) = self.calls.get_mut(key) {
                        partial
                            .arguments
                            .push_str(event["delta"].as_str().unwrap_or_default());
                    }
                }
                "response.completed" => {
                    let response = &event["response"];
                    self.usage.input_tokens = response["usage"]["input_tokens"]
                        .as_u64()
                        .unwrap_or(self.usage.input_tokens);
                    self.usage.output_tokens = response["usage"]["output_tokens"]
                        .as_u64()
                        .unwrap_or(self.usage.output_tokens);
                }
                "response.failed" => {
                    return Err(ModelError::Fatal(format!(
                        "PersonAI Responses failed: {}",
                        event["response"]["error"]
                    )));
                }
                _ => {}
            },
        }
        Ok(())
    }

    fn finish(self) -> Result<ChatResponse, ModelError> {
        let mut result = ChatResponse::text(self.text);
        result.usage = self.usage;
        for (_, call) in self.calls {
            let arguments = serde_json::from_str(if call.arguments.is_empty() {
                "{}"
            } else {
                &call.arguments
            })
            .map_err(|error| {
                ModelError::Fatal(format!("invalid streamed tool arguments: {error}"))
            })?;
            result
                .tool_calls
                .push(ToolCall::new(call.id, call.name, arguments));
        }
        Ok(result)
    }
}

#[async_trait]
impl RerankModel for BoundPersonAiModels {
    async fn rerank(
        &self,
        model: &str,
        request: RerankRequest,
    ) -> Result<RerankResponse, ModelError> {
        Ok(self.rerank_with_usage(model, request, None).await?.response)
    }

    async fn rerank_with_usage(
        &self,
        model: &str,
        request: RerankRequest,
        _: Option<u32>,
    ) -> Result<RerankModelOutput, ModelError> {
        self.ensure_current()?;
        if std::iter::once(&request.query)
            .chain(request.documents.iter())
            .any(|content| {
                content
                    .0
                    .iter()
                    .any(|block| !matches!(block, ContentBlock::Text { .. }))
            })
        {
            return Err(ModelError::Fatal(
                "PersonAI rerank accepts text only".into(),
            ));
        }
        let response = self
            .services
            .rerank
            .rerank_typed(
                &self.account.authorization,
                &crab_serverapi::gateway::RerankRequest {
                    model: None,
                    model_id: Some(model.to_owned()),
                    query: request.query.text(),
                    documents: request
                        .documents
                        .iter()
                        .map(|content| {
                            crab_serverapi::gateway::RerankDocument::Text(content.text())
                        })
                        .collect(),
                    top_n: request.top_n,
                    return_documents: Some(false),
                },
            )
            .await
            .map_err(|error| self.provider_error(error, model))?;
        Ok(RerankModelOutput {
            response: RerankResponse {
                model: self.model_ref(model),
                results: response
                    .results
                    .into_iter()
                    .map(|result| RankedDocument {
                        index: result.index,
                        score: result.relevance_score,
                    })
                    .collect(),
            },
            usage: response
                .usage
                .and_then(|usage| usage.total_tokens)
                .map(|input_tokens| Usage {
                    input_tokens,
                    output_tokens: 0,
                }),
        })
    }
}

#[async_trait]
impl EmbeddingModel for BoundPersonAiModels {
    async fn embed(
        &self,
        model: &str,
        request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, ModelError> {
        self.ensure_current()?;
        let inputs: Vec<crab_serverapi::gateway::EmbeddingInput> = request
            .inputs
            .iter()
            .map(personai_embedding_input)
            .collect::<Result<_, _>>()?;
        let mut body = serde_json::to_value(crab_serverapi::gateway::EmbeddingsRequest {
            model: model.into(),
            input: inputs,
        })
        .map_err(|error| ModelError::Fatal(error.to_string()))?;
        if let Some(dimensions) = request.dimensions {
            body["dimensions"] = json!(dimensions);
        }
        let raw =
            serde_json::to_string(&body).map_err(|error| ModelError::Fatal(error.to_string()))?;
        let response = self
            .services
            .embeddings
            .create(&self.account.authorization, &raw)
            .await
            .map_err(|error| self.provider_error(error, model))?;
        let vectors = response["data"]
            .as_array()
            .ok_or_else(|| {
                ModelError::Fatal("PersonAI embedding response has no data array".into())
            })?
            .iter()
            .map(|item| {
                let index = item["index"]
                    .as_u64()
                    .ok_or_else(|| ModelError::Fatal("embedding index missing".into()))?
                    as usize;
                let values = item["embedding"]
                    .as_array()
                    .ok_or_else(|| ModelError::Fatal("embedding vector missing".into()))?
                    .iter()
                    .map(|number| {
                        number
                            .as_f64()
                            .map(|value| value as f32)
                            .ok_or_else(|| ModelError::Fatal("invalid embedding value".into()))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(EmbeddingVector { index, values })
            })
            .collect::<Result<Vec<_>, ModelError>>()?;
        let result = EmbeddingResponse {
            provider_model: response
                .get("model")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            model: self.model_ref(model),
            vectors,
            input_tokens: response["usage"]["prompt_tokens"].as_u64(),
        };
        result.validate(request.inputs.len(), request.dimensions)?;
        Ok(result)
    }
}

/// Preserve caller-selected reasoning controls. Absence of an opt-in does not
/// disable forced-reasoning models or select a lower effort on their behalf.
pub fn personai_generation_request_body(body: &Value, thinking_enabled: bool) -> Value {
    let mut body = body.clone();
    if thinking_enabled {
        if body.get("enable_thinking").is_none() {
            body["enable_thinking"] = json!(true);
        }
        if body.get("thinking").is_none() {
            body["thinking"] = json!({"type":"enabled"});
        }
    }
    body
}

/// Encode ordered text/image inputs using PersonAI's native embedding contract.
/// Images must already be resolved by the authorized host.
pub fn personai_embedding_request_body(
    model: &str,
    request: &EmbeddingRequest,
) -> Result<Value, ModelError> {
    let inputs = request
        .inputs
        .iter()
        .map(personai_embedding_input)
        .collect::<Result<Vec<_>, _>>()?;
    let mut body = serde_json::to_value(crab_serverapi::gateway::EmbeddingsRequest {
        model: model.into(),
        input: inputs,
    })
    .map_err(|error| ModelError::Fatal(error.to_string()))?;
    if let Some(dimensions) = request.dimensions {
        body["dimensions"] = json!(dimensions);
    }
    Ok(body)
}

fn personai_embedding_input(
    content: &Content,
) -> Result<crab_serverapi::gateway::EmbeddingInput, ModelError> {
    use crab_serverapi::gateway::{EmbeddingImageInput, EmbeddingInput};
    let mut image = None;
    for block in &content.0 {
        match block {
            ContentBlock::Text { .. } => {}
            ContentBlock::Media { media }
                if media.mime_type.starts_with("image/") && image.is_none() =>
            {
                image = Some(media)
            }
            _ => {
                return Err(ModelError::Fatal(
                    "PersonAI embedding input accepts text and at most one resolved image per item"
                        .into(),
                ));
            }
        }
    }
    let text = content.text();
    let Some(image) = image else {
        if text.trim().is_empty() {
            return Err(ModelError::Fatal("embedding text must not be empty".into()));
        }
        return Ok(EmbeddingInput::Text(text));
    };
    let (image_url, image_base64) = match &image.source {
        MediaSource::Url(url) if !url.trim().is_empty() => (Some(url.clone()), None),
        MediaSource::Base64(data) if !data.trim().is_empty() => (None, Some(data.clone())),
        _ => {
            return Err(ModelError::Fatal(
                "embedding image must not be empty".into(),
            ));
        }
    };
    Ok(EmbeddingInput::Image(EmbeddingImageInput {
        text: (!text.is_empty()).then_some(text),
        image_url,
        image_base64,
    }))
}

#[async_trait]
impl ImageGenerationModel for BoundPersonAiModels {
    async fn generate(
        &self,
        _model: &str,
        _request: ImageGenerationRequest,
    ) -> Result<ImageGenerationResponse, ModelError> {
        Err(ModelError::Fatal(
            "PersonAI image generation is asynchronous; use submit_image/query_image".into(),
        ))
    }

    async fn submit(
        &self,
        model: &str,
        request: ImageGenerationRequest,
    ) -> Result<ImageJob, ModelError> {
        self.ensure_current()?;
        let count = i32::try_from(request.count)
            .map_err(|_| ModelError::Fatal("image count exceeds PersonAI request range".into()))?;
        let sdk_request = multimedia::ImageGenerationRequest {
            prompt: request.prompt,
            model_id: Some(model.into()),
            size: request.size,
            n: Some(count),
            quality: request.quality,
            ..Default::default()
        };
        let response = if request.provider_options.is_empty() {
            self.services
                .multimedia
                .generate_image(&self.account.authorization, &sdk_request)
                .await
                .map_err(|error| self.provider_error(error, model))?
        } else {
            let mut body = serde_json::to_value(&sdk_request)
                .map_err(|error| ModelError::Fatal(error.to_string()))?;
            for (key, value) in request.provider_options {
                if body.get(&key).is_some()
                    || matches!(
                        key.as_str(),
                        "prompt" | "model_id" | "size" | "n" | "quality"
                    )
                {
                    return Err(ModelError::Fatal(format!(
                        "PersonAI provider option overrides {key}"
                    )));
                }
                body[&key] = value;
            }
            let http = self
                .api
                .new_authorized_request(
                    Method::POST,
                    "/images/generations",
                    &self.account.authorization,
                )
                .map_err(|error| self.provider_error(error, model))?
                .json(&body);
            self.api
                .execute_for_object::<multimedia::MultimediaTaskResponse>(
                    http,
                    Some(&self.account.authorization),
                    "PersonAI image generation",
                )
                .await
                .map_err(|error| self.provider_error(error, model))?
        };
        Ok(self.image_job(model, response))
    }

    async fn query(&self, job: &ImageJob) -> Result<ImageJob, ModelError> {
        self.ensure_open()
            .map_err(|error| error.with_model(job.model.clone()))?;
        let response = self
            .services
            .multimedia
            .get_task(&self.account.authorization, &job.id)
            .await
            .map_err(|error| self.provider_error(error, &job.model.model))?;
        let mut updated = self.image_job(&job.model.model, response);
        updated.binding_id = job.binding_id;
        Ok(updated)
    }

    async fn cancel(&self, _job: &ImageJob) -> Result<(), ModelError> {
        Err(ModelError::Fatal(
            "PersonAI image task cancellation is unsupported by the SDK".into(),
        ))
    }
}

#[async_trait]
impl VideoGenerationModel for BoundPersonAiModels {
    async fn submit(
        &self,
        model: &str,
        request: VideoGenerationRequest,
    ) -> Result<VideoJob, ModelError> {
        self.ensure_current()?;
        let duration = i32::try_from(request.seconds).map_err(|_| {
            ModelError::Fatal("video duration exceeds PersonAI request range".into())
        })?;
        let mut extra_params = serde_json::Map::new();
        for (key, value) in request.provider_options {
            if matches!(
                key.as_str(),
                "prompt"
                    | "model_id"
                    | "duration"
                    | "resolution"
                    | "ratio"
                    | "first_frame"
                    | "last_frame"
                    | "reference_images"
                    | "reference_videos"
                    | "reference_audios"
            ) {
                return Err(ModelError::Fatal(format!(
                    "PersonAI provider option overrides {key}"
                )));
            }
            extra_params.insert(key, value);
        }
        let sdk_request = multimedia::VideoGenerationRequest {
            prompt: request.prompt,
            model_id: Some(model.into()),
            duration: Some(duration),
            resolution: request.size,
            ratio: None,
            first_frame: request.first_frame.as_ref().map(media_url).transpose()?,
            last_frame: request.last_frame.as_ref().map(media_url).transpose()?,
            reference_images: Some(
                request
                    .reference_images
                    .iter()
                    .map(media_url)
                    .collect::<Result<_, _>>()?,
            ),
            reference_videos: Some(
                request
                    .reference_videos
                    .iter()
                    .map(media_url)
                    .collect::<Result<_, _>>()?,
            ),
            reference_audios: Some(
                request
                    .reference_audios
                    .iter()
                    .map(media_url)
                    .collect::<Result<_, _>>()?,
            ),
            img_url: None,
            video_url: None,
            audio_url: None,
            negative_prompt: None,
            input: None,
            extra_params: Some(extra_params),
        };
        let response = self
            .services
            .multimedia
            .generate_video(&self.account.authorization, &sdk_request)
            .await
            .map_err(|error| self.provider_error(error, model))?;
        Ok(self.video_job(model, response))
    }

    async fn query(&self, job: &VideoJob) -> Result<VideoJob, ModelError> {
        self.ensure_open()
            .map_err(|error| error.with_model(job.model.clone()))?;
        let response = self
            .services
            .multimedia
            .get_task(&self.account.authorization, &job.id)
            .await
            .map_err(|error| self.provider_error(error, &job.model.model))?;
        let mut updated = self.video_job(&job.model.model, response);
        updated.binding_id = job.binding_id;
        Ok(updated)
    }

    async fn cancel(&self, _job: &VideoJob) -> Result<(), ModelError> {
        Err(ModelError::Fatal(
            "PersonAI video task cancellation is unsupported by the SDK".into(),
        ))
    }
}

fn media_url(media: &MediaContent) -> Result<String, ModelError> {
    match &media.source {
        MediaSource::Url(url) => Ok(url.clone()),
        MediaSource::Base64(_) => Err(ModelError::Fatal(
            "PersonAI video references require URLs".into(),
        )),
    }
}

impl BoundPersonAiModels {
    fn image_job(&self, model: &str, task: multimedia::MultimediaTaskResponse) -> ImageJob {
        let images = task
            .result_url
            .map(|url| MediaContent {
                mime_type: "image/*".into(),
                source: MediaSource::Url(url),
                name: None,
                estimated_tokens: None,
            })
            .into_iter()
            .collect();
        ImageJob {
            model: self.model_ref(model),
            protocol: "personai.multimedia".into(),
            id: task.id,
            binding_id: None,
            status: job_status(task.status),
            progress: Some((task.progress as f64 / 100.0).clamp(0.0, 1.0)),
            images,
            error: task.error_message,
        }
    }

    fn video_job(&self, model: &str, task: multimedia::MultimediaTaskResponse) -> VideoJob {
        VideoJob {
            model: self.model_ref(model),
            protocol: "personai.multimedia".into(),
            id: task.id,
            binding_id: None,
            status: job_status(task.status),
            progress: Some((task.progress as f64 / 100.0).clamp(0.0, 1.0)),
            output: task.result_url.map(|uri| ResourceReference {
                uri,
                mime_type: Some("video/mp4".into()),
                name: None,
                size_bytes: None,
            }),
            error: task.error_message,
        }
    }
}

#[async_trait]
impl ContentDetectionModel for BoundPersonAiModels {
    async fn detect(
        &self,
        model: &str,
        request: ContentDetectionRequest,
    ) -> Result<ContentDetectionResponse, ModelError> {
        self.ensure_current()?;
        let media = request.content.0.iter().find_map(|block| match block {
            ContentBlock::Media { media } => Some(media),
            _ => None,
        });
        let result = if let Some(media) = media {
            if !media.mime_type.starts_with("image/") {
                return Err(ModelError::Fatal(
                    "PersonAI AIGC detection accepts text or images".into(),
                ));
            }
            let (image_url, image_base64) = match &media.source {
                MediaSource::Url(url) => (Some(url.clone()), None),
                MediaSource::Base64(encoded) => (None, Some(encoded.clone())),
            };
            self.services
                .multimedia
                .detect_image(
                    &self.account.authorization,
                    &multimedia::AigcImageDetectionRequest {
                        image_url,
                        image_base64,
                        model_id: Some(model.into()),
                    },
                )
                .await
                .map_err(|error| self.provider_error(error, model))?
        } else {
            self.services
                .multimedia
                .detect_text(
                    &self.account.authorization,
                    &multimedia::AigcTextDetectionRequest {
                        text: request.content.text(),
                        is_merge: Some(true),
                        model_id: Some(model.into()),
                    },
                )
                .await
                .map_err(|error| self.provider_error(error, model))?
        };
        let segments: Vec<Value> = result
            .segment_labels
            .unwrap_or_default()
            .into_iter()
            .map(|segment| {
                json!({
                    "text": segment.text, "label": segment.label, "confidence": segment.conf,
                    "order": segment.order, "position": segment.position,
                })
            })
            .collect();
        let result = json!({
            "id": result.id, "detection_type": result.detection_type,
            "model_code": result.model_code, "status": result.status,
            "ai_confidence": result.ai_confidence, "human_ratio": result.human_ratio,
            "ai_ratio": result.ai_ratio, "suspected_ai_ratio": result.suspected_ai_ratio,
            "ratio_confidence": result.ratio_confidence, "segment_labels": segments,
            "input_text": result.input_text, "image_url": result.image_url,
            "image_digest": result.image_digest, "is_merge": result.is_merge,
            "tokens_used": result.tokens_used, "points_cost": result.points_cost,
            "last_error": result.last_error, "created_at": result.created_at,
            "updated_at": result.updated_at,
        });
        Ok(ContentDetectionResponse {
            model: self.model_ref(model),
            result,
        })
    }
}

#[async_trait]
impl SpeechRecognitionModel for BoundPersonAiModels {
    async fn transcribe(
        &self,
        model: &str,
        request: SpeechRecognitionRequest,
    ) -> Result<SpeechRecognitionResponse, ModelError> {
        self.ensure_current()?;
        if model != "speech-auto" {
            return Err(ModelError::Fatal("PersonAI transcription is server-selected; use speech-auto".into()));
        }
        let response = crate::speech::PersonAiSpeechRecognitionModel {
            client: self.services.audio_realtime.clone(),
            authorization: self.account.authorization.clone(),
        }.transcribe(model, request).await?;
        self.ensure_current()?;
        Ok(response)
    }
}

fn job_status(status: i32) -> ModelJobStatus {
    match status {
        TaskStatus::PENDING => ModelJobStatus::Queued,
        TaskStatus::PROCESSING => ModelJobStatus::Running,
        TaskStatus::SUCCEEDED => ModelJobStatus::Completed,
        _ => ModelJobStatus::Failed,
    }
}

fn map_error(error: ServerApiError) -> ModelError {
    use ProviderErrorCategory as Category;
    let mut detail = ProviderModelError {
        authorization_attempt: None,
        provider: "personai".into(),
        category: Category::Other,
        http_status: None,
        business_code: None,
        error_type: None,
        model: None,
        message: error.to_string(),
    };
    match &error {
        ServerApiError::Gateway {
            status,
            message,
            error_type,
        } => {
            detail.http_status = Some(*status);
            detail.message = message.clone();
            detail.error_type = Some(error_type.clone());
            let message_lower = message.to_ascii_lowercase();
            let rate_limited = matches!(
                error_type.as_str(),
                "1302" | "rate_limit_exceeded" | "rate_limit_error"
            ) || message_lower.contains("速率限制")
                || message_lower.contains("请求频率")
                || message_lower.contains("rate limit");
            detail.category = match (*status, error_type.as_str()) {
                (401, _) => Category::Authentication,
                (403, _) => Category::Permission,
                (404, "gateway_error" | "model_not_found") => Category::Unavailable,
                (429, _) if rate_limited => Category::Transient,
                (429, "gateway_error" | "insufficient_quota" | "insufficient_balance") => {
                    Category::Quota
                }
                (429, _) => Category::Transient,
                (502..=504, _) => Category::Transient,
                _ => Category::Other,
            };
        }
        ServerApiError::Business { code, message } => {
            detail.business_code = Some(*code);
            detail.message = message.clone();
            detail.category = match code {
                401 => Category::Authentication,
                403 => Category::Permission,
                429 if message.contains("速率限制")
                    || message.contains("请求频率")
                    || message.to_ascii_lowercase().contains("rate limit") =>
                {
                    Category::Transient
                }
                429 => Category::Quota,
                _ => Category::Other,
            };
        }
        ServerApiError::DecryptFailed | ServerApiError::DeviceUnbound => {
            detail.category = Category::Authentication;
        }
        ServerApiError::Http(error) => {
            detail.http_status = error.status().map(|status| status.as_u16());
            detail.category = if error.is_connect() || error.is_timeout() {
                Category::Transient
            } else {
                Category::Other
            };
        }
        ServerApiError::WebSocket(_) => detail.category = Category::Transient,
        _ => {}
    }
    ModelError::Provider(Box::new(detail))
}

fn chat_completions_request(model: &str, request: &ChatRequest) -> Result<Value, ModelError> {
    let messages = request.messages.iter().map(|message| {
        let role = match message.role {
            Role::System => "system", Role::User => "user",
            Role::Assistant => "assistant", Role::Tool => "tool",
        };
        let mut encoded = json!({"role": role, "content": content_for_chat(&message.content)?});
        if let Some(id) = &message.tool_call_id { encoded["tool_call_id"] = json!(id); }
        if !message.tool_calls.is_empty() {
            encoded["tool_calls"] = Value::Array(message.tool_calls.iter().map(|call| json!({
                "id": call.id, "type": "function", "function": {"name": call.call_name, "arguments": call.arguments.to_string()}
            })).collect());
        }
        Ok(encoded)
    }).collect::<Result<Vec<_>, ModelError>>()?;
    let mut body = json!({"model": model, "messages": messages});
    apply_options(&mut body, request, false);
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(request.tools.iter().map(|tool| json!({
            "type": "function", "function": {"name": tool.call_name, "description": tool.description, "parameters": tool.input_schema}
        })).collect());
    } else {
        body["tool_choice"] = json!("none");
    }
    Ok(body)
}

fn responses_request(model: &str, request: &ChatRequest) -> Result<Value, ModelError> {
    if request.options.stop_sequences.is_some() {
        return Err(ModelError::Fatal(
            "PersonAI Responses does not accept stop_sequences".into(),
        ));
    }
    let mut input = Vec::new();
    for message in &request.messages {
        if message.role == Role::Tool {
            if message.content.has_media() {
                return Err(ModelError::Fatal(
                    "PersonAI Responses tool results must be text".into(),
                ));
            }
            input.push(json!({"type":"function_call_output",
                "call_id": message.tool_call_id.as_deref().unwrap_or_default(),
                "output": message.content.text()}));
            continue;
        }
        let role = match message.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => unreachable!(),
        };
        if !message.content.is_empty() {
            input.push(json!({"role": role, "content": content_for_responses(&message.content)?}));
        }
        for call in &message.tool_calls {
            input.push(json!({"type":"function_call", "call_id":call.id,
                "name":call.call_name, "arguments":call.arguments.to_string()}));
        }
    }
    let mut body = json!({"model": model, "input": input});
    apply_options(&mut body, request, true);
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(request.tools.iter().map(|tool| json!({
            "type": "function", "name": tool.call_name, "description": tool.description, "parameters": tool.input_schema
        })).collect());
    } else {
        body["tool_choice"] = json!("none");
    }
    Ok(body)
}

fn apply_options(body: &mut Value, request: &ChatRequest, responses: bool) {
    if let Some(value) = request.options.temperature {
        body["temperature"] = json!(value);
    }
    if let Some(value) = request.options.top_p {
        body["top_p"] = json!(value);
    }
    if let Some(value) = request.options.max_output_tokens {
        body[if responses {
            "max_output_tokens"
        } else {
            // PersonAI's Chat gateway defaults max_tokens when it is absent.
            // Sending only max_completion_tokens leaves two competing limits.
            "max_tokens"
        }] = json!(value);
    }
    if let Some(value) = &request.options.stop_sequences
        && !responses
    {
        body["stop"] = json!(value);
    }
    if let Some(schema) = &request.options.output_schema {
        if responses {
            body["text"]["format"] = json!({"type":"json_schema", "name":schema.name,
                "schema":schema.schema, "strict":true});
        } else {
            body["response_format"] = json!({"type":"json_schema", "json_schema": {
                "name":schema.name, "schema":schema.schema, "strict":true
            }});
        }
    }
}

fn content_for_chat(content: &Content) -> Result<Value, ModelError> {
    if content
        .0
        .iter()
        .any(|block| matches!(block, ContentBlock::Resource { .. }))
    {
        return Err(ModelError::Fatal(
            "PersonAI chat cannot resolve opaque resources".into(),
        ));
    }
    if !content.has_media() {
        return Ok(json!(content.text()));
    }
    let mut blocks = Vec::new();
    for block in &content.0 {
        match block {
            ContentBlock::Text { text } => blocks.push(json!({"type": "text", "text": text})),
            ContentBlock::Json { value } => {
                blocks.push(json!({"type": "text", "text": value.to_string()}))
            }
            ContentBlock::Media { media } if media.mime_type.starts_with("image/") => {
                let uri = match &media.source {
                    MediaSource::Url(uri) => uri.clone(),
                    MediaSource::Base64(data) => format!("data:{};base64,{data}", media.mime_type),
                };
                blocks.push(json!({"type": "image_url", "image_url": {"url": uri}}));
            }
            ContentBlock::Media { .. } | ContentBlock::Resource { .. } => {
                return Err(ModelError::Fatal(
                    "PersonAI chat adapter cannot inline this content block".into(),
                ));
            }
        }
    }
    Ok(Value::Array(blocks))
}

fn content_for_responses(content: &Content) -> Result<Value, ModelError> {
    let mut blocks = Vec::new();
    for block in &content.0 {
        match block {
            ContentBlock::Text { text } => blocks.push(json!({"type": "input_text", "text": text})),
            ContentBlock::Json { value } => {
                blocks.push(json!({"type": "input_text", "text": value.to_string()}))
            }
            ContentBlock::Media { media } if media.mime_type.starts_with("image/") => {
                let uri = match &media.source {
                    MediaSource::Url(uri) => uri.clone(),
                    MediaSource::Base64(data) => format!("data:{};base64,{data}", media.mime_type),
                };
                blocks.push(json!({"type": "input_image", "image_url": uri}));
            }
            ContentBlock::Media { .. } | ContentBlock::Resource { .. } => {
                return Err(ModelError::Fatal(
                    "PersonAI Responses cannot inline this content block".into(),
                ));
            }
        }
    }
    Ok(Value::Array(blocks))
}

fn parse_chat_response(response: Value) -> Result<ChatResponse, ModelError> {
    let message = response["choices"][0]["message"]
        .as_object()
        .ok_or_else(|| ModelError::Fatal("PersonAI Chat response has no message".into()))?;
    let content = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut result = ChatResponse::text(content);
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let id = call["id"].as_str().unwrap_or_default();
            let name = call["function"]["name"].as_str().unwrap_or_default();
            let args = call["function"]["arguments"].as_str().unwrap_or("{}");
            let arguments = serde_json::from_str(args).map_err(|error| {
                ModelError::Fatal(format!("invalid PersonAI tool arguments: {error}"))
            })?;
            result.tool_calls.push(ToolCall::new(id, name, arguments));
        }
    }
    result.usage = Usage {
        input_tokens: response["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
        output_tokens: response["usage"]["completion_tokens"].as_u64().unwrap_or(0),
    };
    Ok(result)
}

fn parse_responses_response(response: Value) -> Result<ChatResponse, ModelError> {
    let mut text = response["output_text"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let has_output_text = !text.is_empty();
    let mut calls = Vec::new();
    if let Some(output) = response["output"].as_array() {
        for item in output {
            if item["type"] == "function_call" {
                let arguments = serde_json::from_str(item["arguments"].as_str().unwrap_or("{}"))
                    .map_err(|error| {
                        ModelError::Fatal(format!("invalid PersonAI function arguments: {error}"))
                    })?;
                calls.push(ToolCall::new(
                    item["call_id"].as_str().unwrap_or_default(),
                    item["name"].as_str().unwrap_or_default(),
                    arguments,
                ));
            } else if !has_output_text && item["type"] == "message" && let Some(content) = item["content"].as_array() {
                for part in content {
                    if let Some(fragment) = part["text"].as_str() {
                        text.push_str(fragment);
                    }
                }
            }
        }
    }
    let mut result = ChatResponse::text(text);
    result.tool_calls = calls;
    result.usage = Usage {
        input_tokens: response["usage"]["input_tokens"].as_u64().unwrap_or(0),
        output_tokens: response["usage"]["output_tokens"].as_u64().unwrap_or(0),
    };
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PersonAiContextHandle;
    use crab_serverapi::base::AuthorizedClientContext;
    use rsa::{
        RsaPrivateKey,
        pkcs8::{EncodePrivateKey, EncodePublicKey},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::{Mutex, oneshot},
    };

    #[test]
    fn provider_errors_preserve_codes_without_treating_every_404_as_model_removal() {
        for (status, kind, expected) in [
            (401, "gateway_error", ProviderErrorCategory::Authentication),
            (403, "gateway_error", ProviderErrorCategory::Permission),
            (404, "gateway_error", ProviderErrorCategory::Unavailable),
            (404, "invalid_request_error", ProviderErrorCategory::Other),
            (429, "gateway_error", ProviderErrorCategory::Quota),
            (429, "1302", ProviderErrorCategory::Transient),
            (429, "rate_limit_exceeded", ProviderErrorCategory::Transient),
            (429, "insufficient_quota", ProviderErrorCategory::Quota),
            (502, "gateway_error", ProviderErrorCategory::Transient),
        ] {
            let error = map_error(ServerApiError::gateway(status, "original message", kind))
                .with_model(ModelRef::new("personai", "p/model"));
            assert_eq!(
                error.is_transient(),
                expected == ProviderErrorCategory::Transient
            );
            assert_eq!(
                error.is_unavailable(),
                expected == ProviderErrorCategory::Unavailable
            );
            let ModelError::Provider(detail) = error else {
                panic!("structured error expected")
            };
            assert_eq!(detail.category, expected);
            assert_eq!(detail.http_status, Some(status));
            assert_eq!(detail.error_type.as_deref(), Some(kind));
            assert_eq!(detail.message, "original message");
            assert_eq!(detail.model, Some(ModelRef::new("personai", "p/model")));
        }
        let ModelError::Provider(detail) = map_error(ServerApiError::business(429, "quota")) else {
            panic!("structured error expected")
        };
        assert_eq!(detail.business_code, Some(429));
        assert_eq!(detail.category, ProviderErrorCategory::Quota);
    }

    #[test]
    fn provider_rate_limit_wrapped_as_gateway_error_remains_transient() {
        let ModelError::Provider(detail) = map_error(ServerApiError::gateway(
            429,
            "您的账户已达到速率限制，请您控制请求频率",
            "gateway_error",
        )) else {
            panic!("structured error expected")
        };
        assert_eq!(detail.category, ProviderErrorCategory::Transient);
        let ModelError::Provider(detail) = map_error(ServerApiError::business(
            429,
            "您的账户已达到速率限制，请您控制请求频率",
        )) else {
            panic!("structured error expected")
        };
        assert_eq!(detail.category, ProviderErrorCategory::Transient);
    }

    #[test]
    fn responses_preserve_text_from_every_output_message_without_duplicating_summary() {
        let output = json!([
            {"type":"message","content":[{"type":"output_text","text":"first "}]},
            {"type":"function_call","call_id":"call-1","name":"lookup","arguments":"{}"},
            {"type":"message","content":[
                {"type":"output_text","text":"second "},
                {"type":"output_text","text":"third"}
            ]}
        ]);
        for summary in [None, Some(""), Some("first second third")] {
            let mut raw = json!({"output": output.clone()});
            if let Some(summary) = summary {
                raw["output_text"] = summary.into();
            }
            let parsed = parse_responses_response(raw).unwrap();
            assert_eq!(parsed.content.text(), "first second third");
            assert_eq!(parsed.tool_calls.len(), 1);
            assert_eq!(parsed.tool_calls[0].call_name, "lookup");
        }
    }

    #[derive(Default)]
    struct CollectedStream(Mutex<Vec<String>>);
    #[async_trait]
    impl ChatStream for CollectedStream {
        async fn text_delta(&self, text: &str) {
            self.0.lock().await.push(text.into());
        }
        async fn reasoning_delta(&self, _text: &str) {}
    }

    fn chat_request() -> ChatRequest {
        ChatRequest {
            options: Default::default(),
            messages: vec![
                eliteforge_agent_core::Message::system("trusted host rule"),
                eliteforge_agent_core::Message::user("hello"),
            ],
            tools: vec![],
            extensions: Default::default(),
        }
    }

    #[test]
    fn native_embedding_discovery_uses_tags_and_keeps_instance_metadata_in_cache() {
        let provider = Arc::new(PersonAiContextHandle::default());
        provider.publish(AccountContext {
            account_scope: "alice".into(),
            revision: 1,
            authorization: AuthorizedClientContext::new(),
        });
        let url = "http://127.0.0.1:1";
        let factory = PersonAiModels::new(
            "personai".into(),
            url,
            Arc::new(PersonAiServices::new(url).unwrap()),
            provider.clone(),
        )
        .unwrap();
        let media = |id: &str, code: &str, tags: &[&str]| DiscoveredMediaModel {
            id: id.into(),
            model_code: code.into(),
            model_name: None,
            media_type: multimedia::MediaType::EMBEDDING,
            dimension: Some(1024),
            tags: tags.iter().map(|tag| (*tag).into()).collect(),
        };
        let snapshot = PersonAiDiscoverySnapshot {
            account_scope: "alice".into(),
            account_revision: 1,
            media: BTreeMap::from([(
                multimedia::MediaType::EMBEDDING,
                vec![
                    media("text-id", "same-code", &["text_embedding"]),
                    media("image-id", "same-code", &["multimodal_embedding"]),
                    media("unknown-id", "Qwen3-VL-Embedding-2B", &[]),
                ],
            )]),
            ..Default::default()
        };
        let config = PersonAiConfig::new(url);
        let update = factory
            .runtime_update(provider.current().unwrap(), &snapshot, &config)
            .unwrap();
        assert_eq!(update.embedding.len(), 2);
        assert_eq!(update.embedding[0].model.model, "text-id");
        assert_eq!(update.embedding[0].capabilities.input, [Modality::Text]);
        assert_eq!(update.embedding[1].model.model, "image-id");
        assert_eq!(
            update.embedding[1].capabilities.input,
            [Modality::Text, Modality::Image]
        );
        let cached = factory.catalog_snapshot(&snapshot, &config);
        assert_eq!(cached.entries.len(), 2);
        let restored = restore_discovery(&cached).unwrap();
        let image = &restored.media[&multimedia::MediaType::EMBEDDING][1];
        assert_eq!(image.dimension, Some(1024));
        assert_eq!(image.tags, ["multimodal_embedding"]);
    }

    #[test]
    fn native_rerank_discovery_registers_typed_model_and_catalog() {
        let provider = Arc::new(PersonAiContextHandle::default());
        provider.publish(AccountContext {
            account_scope: "alice".into(),
            revision: 1,
            authorization: AuthorizedClientContext::new(),
        });
        let url = "http://127.0.0.1:1";
        let factory = PersonAiModels::new(
            "personai".into(),
            url,
            Arc::new(PersonAiServices::new(url).unwrap()),
            provider.clone(),
        )
        .unwrap();
        let mut config = PersonAiConfig::new(url);
        config.embedding_models = vec!["text-only".into(), "image-space".into()];
        config.multimodal_embedding_models = vec!["image-space".into()];
        let snapshot = PersonAiDiscoverySnapshot {
            account_scope: "alice".into(),
            account_revision: 1,
            media: BTreeMap::from([(
                PERSONAI_RERANK_MEDIA_TYPE,
                vec![DiscoveredMediaModel {
                    id: "rerank-1".into(),
                    model_code: "bge-reranker".into(),
                    model_name: None,
                    media_type: PERSONAI_RERANK_MEDIA_TYPE,
                    tags: vec!["rerank".into()],
                    dimension: None,
                }],
            )]),
            ..Default::default()
        };
        let update = factory
            .runtime_update(provider.current().unwrap(), &snapshot, &config)
            .unwrap();
        assert_eq!(update.embedding.len(), 2);
        assert_eq!(update.embedding[0].capabilities.input, vec![Modality::Text]);
        assert_eq!(update.embedding[1].model.model, "image-space");
        assert_eq!(
            update.embedding[1].capabilities.input,
            vec![Modality::Text, Modality::Image]
        );
        assert_eq!(update.rerank.len(), 1);
        assert_eq!(
            update.rerank[0].model,
            ModelRef::new("personai", "rerank-1")
        );
        let catalog = factory.catalog_snapshot(&snapshot, &config);
        assert_eq!(catalog.entries[0].operation, ModelOperation::Rerank);
        assert_eq!(catalog.entries[0].protocol, "personai.rerank");
        assert_eq!(
            restore_discovery(&catalog).unwrap().media[&PERSONAI_RERANK_MEDIA_TYPE][0].id,
            "rerank-1"
        );
    }

    #[tokio::test]
    async fn streams_chat_while_account_change_does_not_rebind_inflight_call() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (accepted_tx, accepted_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let private = RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap();
        let public = private
            .to_public_key()
            .to_public_key_pem(rsa::pkcs8::LineEnding::LF)
            .unwrap();
        let private_pem = private.to_pkcs8_pem(rsa::pkcs8::LineEnding::LF).unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let count = socket.read(&mut chunk).await.unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..count]);
                let header_end = request.windows(4).position(|bytes| bytes == b"\r\n\r\n");
                if let Some(end) = header_end {
                    let headers = String::from_utf8_lossy(&request[..end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let end = request
                .windows(4)
                .position(|bytes| bytes == b"\r\n\r\n")
                .unwrap()
                + 4;
            let envelope: crab_serverapi::crypto::ClientEncryptedEnvelope =
                serde_json::from_slice(&request[end..]).unwrap();
            let plaintext = crab_serverapi::crypto::ClientCrypto::decrypt_to_string(
                private_pem.as_str(),
                &envelope.encrypted_key,
                &envelope.iv,
                &envelope.ciphertext,
            )
            .unwrap();
            let body: Value = serde_json::from_str(&plaintext).unwrap();
            // Assert the actual SDK-encrypted HTTP payload, not just the builder.
            assert_eq!(body["stream"], true);
            assert_eq!(body["stream_options"]["include_usage"], true);
            assert_eq!(body["messages"][0]["role"], "system");
            assert_eq!(body["messages"][0]["content"], "trusted host rule");
            assert_eq!(body["messages"][1]["role"], "user");
            accepted_tx.send(()).unwrap();
            release_rx.await.unwrap();
            let body = concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
                "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}\n\n",
                "data: [DONE]\n\n"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let provider = Arc::new(PersonAiContextHandle::default());
        let auth = AuthorizedClientContext::new()
            .with_token("token")
            .with_client_id("client")
            .with_public_key(public)
            .with_product_code("product")
            .with_product_version("1.0.0");
        provider.publish(AccountContext {
            account_scope: "alice".into(),
            revision: 1,
            authorization: auth.clone(),
        });
        let services = Arc::new(PersonAiServices::new(&url).unwrap());
        let factory =
            PersonAiModels::new("personai".into(), &url, services, provider.clone()).unwrap();
        let binding = factory.bind_current().unwrap();
        let adapter = Arc::new(PersonAiChatAdapter {
            binding,
            protocol: ProtocolType::ChatCompletions,
            quality: QualityType::Low,
        });
        let events = Arc::new(CollectedStream::default());
        let running = {
            let adapter = adapter.clone();
            let events = events.clone();
            tokio::spawn(async move {
                adapter
                    .generate("p/model", chat_request(), events.as_ref())
                    .await
            })
        };
        accepted_rx.await.unwrap();
        provider.publish(AccountContext {
            account_scope: "bob".into(),
            revision: 2,
            authorization: auth,
        });
        release_tx.send(()).unwrap();
        let response = running.await.unwrap().unwrap();
        assert_eq!(response.content.text(), "hello");
        assert_eq!(response.usage.input_tokens, 7);
        assert_eq!(response.usage.output_tokens, 3);
        assert_eq!(events.0.lock().await.as_slice(), ["hel", "lo"]);
        assert!(
            adapter
                .generate("p/model", chat_request(), events.as_ref())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn streams_responses_text_and_tool_calls() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let private = RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap();
        let public = private
            .to_public_key()
            .to_public_key_pem(rsa::pkcs8::LineEnding::LF)
            .unwrap();
        let private_pem = private.to_pkcs8_pem(rsa::pkcs8::LineEnding::LF).unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let count = socket.read(&mut chunk).await.unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..count]);
                let header_end = request.windows(4).position(|bytes| bytes == b"\r\n\r\n");
                if let Some(end) = header_end {
                    let headers = String::from_utf8_lossy(&request[..end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            assert!(String::from_utf8_lossy(&request).contains(" /api/v1/responses "));
            let end = request.windows(4).position(|bytes| bytes == b"\r\n\r\n").unwrap() + 4;
            let envelope: crab_serverapi::crypto::ClientEncryptedEnvelope =
                serde_json::from_slice(&request[end..]).unwrap();
            let plaintext = crab_serverapi::crypto::ClientCrypto::decrypt_to_string(
                private_pem.as_str(), &envelope.encrypted_key, &envelope.iv, &envelope.ciphertext,
            ).unwrap();
            let payload: Value = serde_json::from_str(&plaintext).unwrap();
            assert_eq!(payload["input"][0]["role"], "system");
            assert_eq!(payload["input"][0]["content"][0]["text"], "trusted host rule");
            assert_eq!(payload["input"][1]["role"], "user");
            let body = concat!(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
                "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"item-1\",\"call_id\":\"call-1\",\"name\":\"lookup\"}}\n\n",
                "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"item-1\",\"delta\":\"{\\\"q\\\":\\\"x\\\"}\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":2,\"output_tokens\":3}}}\n\n",
                "data: [DONE]\n\n"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let provider = Arc::new(PersonAiContextHandle::default());
        provider.publish(AccountContext {
            account_scope: "alice".into(),
            revision: 1,
            authorization: AuthorizedClientContext::new()
                .with_token("token")
                .with_client_id("client")
                .with_public_key(public)
                .with_product_code("product")
                .with_product_version("1.0.0"),
        });
        let services = Arc::new(PersonAiServices::new(&url).unwrap());
        let binding = PersonAiModels::new("personai".into(), &url, services, provider)
            .unwrap()
            .bind_current()
            .unwrap();
        let adapter = PersonAiChatAdapter {
            binding,
            protocol: ProtocolType::Responses,
            quality: QualityType::Low,
        };
        let events = CollectedStream::default();
        let result = adapter
            .generate("responses-only", chat_request(), &events)
            .await
            .unwrap();
        assert_eq!(result.content.text(), "hello");
        assert_eq!(result.usage.input_tokens, 2);
        assert_eq!(result.usage.output_tokens, 3);
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].call_name, "lookup");
        assert_eq!(events.0.lock().await.as_slice(), ["hello"]);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn routes_embedding_media_file_speech_and_detection_operations() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for _ in 0..7 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = [0_u8; 8192];
                let size = socket.read(&mut bytes).await.unwrap();
                let request = String::from_utf8_lossy(&bytes[..size]);
                let body = if request.contains(" /api/v1/embeddings ") {
                    r#"{"data":[{"index":0,"embedding":[0.1,0.2]}],"usage":{"prompt_tokens":2}}"#
                } else if request.contains(" /api/v1/rerank ") {
                    r#"{"results":[{"index":1,"relevance_score":0.91}],"usage":{"total_tokens":7}}"#
                } else if request.contains(" /api/v1/images/generations ") {
                    r#"{"code":0,"data":{"id":"img","media_type":0,"status":0,"progress":0}}"#
                } else if request.contains(" /api/v1/multimedia/tasks/img ") {
                    r#"{"code":0,"data":{"id":"img","media_type":0,"status":2,"progress":100,"result_url":"https://example.com/image.png"}}"#
                } else if request.contains(" /api/v1/videos/generations ") {
                    r#"{"code":0,"data":{"id":"vid","media_type":1,"status":0,"progress":0}}"#
                } else if request.contains(" /api/v1/audio/transcriptions ") {
                    r#"{"text":"recognized"}"#
                } else if request.contains(" /api/v1/aigc/detections/text ") {
                    r#"{"code":0,"data":{"id":"det","detection_type":0,"model_code":"text-detector","status":0,"ai_confidence":0.2}}"#
                } else {
                    panic!("unexpected mock request: {request}")
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let private = RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap();
        let public = private
            .to_public_key()
            .to_public_key_pem(rsa::pkcs8::LineEnding::LF)
            .unwrap();
        let provider = Arc::new(PersonAiContextHandle::default());
        provider.publish(AccountContext {
            account_scope: "alice".into(),
            revision: 1,
            authorization: AuthorizedClientContext::new()
                .with_token("token")
                .with_client_id("client")
                .with_public_key(public)
                .with_product_code("product")
                .with_product_version("1.0.0"),
        });
        let services = Arc::new(PersonAiServices::new(&url).unwrap());
        let binding = PersonAiModels::new("personai".into(), &url, services, provider)
            .unwrap()
            .bind_current()
            .unwrap();

        let embedding = EmbeddingModel::embed(
            binding.as_ref(),
            "embedding-1",
            EmbeddingRequest::texts(["hello"]),
        )
        .await
        .unwrap();
        assert_eq!(embedding.vectors[0].values.len(), 2);

        let rerank = RerankModel::rerank_with_usage(
            binding.as_ref(),
            "rerank-1",
            RerankRequest {
                choice: Default::default(),
                query: "orchid".into(),
                documents: vec!["stone".into(), "flower".into()],
                top_n: Some(1),
            },
            None,
        )
        .await
        .unwrap();
        assert_eq!(rerank.response.model, binding.model_ref("rerank-1"));
        assert_eq!(rerank.response.results[0].index, 1);
        assert_eq!(rerank.response.results[0].score, 0.91);
        assert_eq!(rerank.usage.unwrap().input_tokens, 7);
        assert!(
            RerankModel::rerank(
                binding.as_ref(),
                "rerank-1",
                RerankRequest {
                    choice: Default::default(),
                    query: Content(vec![ContentBlock::Json {
                        value: json!({"q":"not text"})
                    }]),
                    documents: vec!["flower".into()],
                    top_n: Some(1),
                }
            )
            .await
            .is_err()
        );

        let image = ImageGenerationModel::submit(
            binding.as_ref(),
            "image-1",
            ImageGenerationRequest {
                choice: Default::default(),
                prompt: "draw".into(),
                size: Some("1024x1024".into()),
                count: 1,
                quality: Some("high".into()),
                provider_options: BTreeMap::from([("seed".into(), json!(7))]),
            },
        )
        .await
        .unwrap();
        assert_eq!(image.id, "img");
        let image = ImageGenerationModel::query(binding.as_ref(), &image)
            .await
            .unwrap();
        assert_eq!(image.status, ModelJobStatus::Completed);
        assert_eq!(image.images.len(), 1);

        let video = VideoGenerationModel::submit(
            binding.as_ref(),
            "video-1",
            VideoGenerationRequest {
                choice: Default::default(),
                prompt: "animate".into(),
                seconds: 5,
                size: Some("720p".into()),
                first_frame: Some(MediaContent {
                    mime_type: "image/png".into(),
                    source: MediaSource::Url("https://example.com/frame.png".into()),
                    name: None,
                    estimated_tokens: Some(1),
                }),
                last_frame: None,
                reference_images: vec![],
                reference_videos: vec![],
                reference_audios: vec![],
                provider_options: BTreeMap::from([("seed".into(), json!(7))]),
            },
        )
        .await
        .unwrap();
        assert_eq!(video.id, "vid");

        let speech = SpeechRecognitionModel::transcribe(
            binding.as_ref(),
            "speech-auto",
            SpeechRecognitionRequest {
                choice: Default::default(),
                audio: MediaContent {
                    mime_type: "audio/wav".into(),
                    source: MediaSource::Base64("AA==".into()),
                    name: Some("clip.wav".into()),
                    estimated_tokens: Some(1),
                },
                language: Some("en".into()),
            },
        )
        .await
        .unwrap();
        assert_eq!(speech.text, "recognized");

        let detection = ContentDetectionModel::detect(
            binding.as_ref(),
            "text-detector",
            ContentDetectionRequest {
                choice: Default::default(),
                content: Content::from("some text"),
            },
        )
        .await
        .unwrap();
        assert_eq!(detection.result["ai_confidence"], 0.2);
        server.await.unwrap();
    }
}

#[cfg(test)]
mod embedding_input_tests {
    use super::*;
    #[test]
    fn native_protocols_preserve_system_instructions_without_role_rewriting() {
        let mut request = ChatRequest {
            options: Default::default(),
            messages: vec![
                eliteforge_agent_core::Message::system("trusted host rule"),
                eliteforge_agent_core::Message::user("question"),
                eliteforge_agent_core::Message::assistant("answer"),
                eliteforge_agent_core::Message::tool("call-1", "evidence"),
            ],
            tools: vec![],
            extensions: Default::default(),
        };
        request.options.max_output_tokens = Some(32_768);
        let chat = chat_completions_request("glm_plan/glm-5.3-flash", &request).unwrap();
        let responses = responses_request("glm_plan/glm-5.3-flash", &request).unwrap();
        assert_eq!(chat["messages"][0]["role"], "system");
        assert_eq!(chat["messages"][0]["content"], "trusted host rule");
        assert_eq!(responses["input"][0]["role"], "system");
        assert_eq!(responses["input"][0]["content"][0]["text"], "trusted host rule");
        assert_eq!(chat["messages"][1]["role"], "user");
        assert_eq!(responses["input"][1]["role"], "user");
        assert_eq!(chat["messages"][2]["role"], "assistant");
        assert_eq!(responses["input"][2]["role"], "assistant");
        assert_eq!(chat["messages"][3]["role"], "tool");
        assert_eq!(chat["messages"][3]["tool_call_id"], "call-1");
        assert_eq!(responses["input"][3]["type"], "function_call_output");
        assert_eq!(chat["max_tokens"], 32_768);
        assert_eq!(responses["max_output_tokens"], 32_768);
    }

    #[test]
    fn native_chat_and_responses_use_their_personai_gateway_output_limit_fields() {
        let mut request = ChatRequest {
            options: Default::default(),
            messages: vec![],
            tools: vec![],
            extensions: Default::default(),
        };
        request.options.max_output_tokens = Some(32_768);
        let chat = chat_completions_request("extractor", &request).unwrap();
        let responses = responses_request("extractor", &request).unwrap();
        assert_eq!(chat["max_tokens"], 32_768);
        assert_eq!(chat["tool_choice"], "none");
        assert!(chat.get("max_completion_tokens").is_none());
        assert!(chat.get("max_output_tokens").is_none());
        assert_eq!(responses["max_output_tokens"], 32_768);
        assert_eq!(responses["tool_choice"], "none");
        assert!(responses.get("max_tokens").is_none());
    }
    #[test]
    fn generation_preserves_provider_defaults_and_explicit_reasoning_controls() {
        let input = json!({"model":"always-reasoning","messages":[{"role":"user","content":"extract graph"}],"max_tokens":2048});
        let lightweight = personai_generation_request_body(&input, false);
        assert_eq!(lightweight, input);
        assert!(lightweight.get("thinking").is_none());
        assert!(lightweight.get("enable_thinking").is_none());
        assert_eq!(lightweight["messages"], input["messages"]);
        assert_eq!(lightweight["max_tokens"], 2048);
        let enabled = personai_generation_request_body(&input, true);
        assert_eq!(enabled["enable_thinking"], true);
        assert_eq!(enabled["thinking"]["type"], "enabled");
        assert_eq!(enabled["messages"], input["messages"]);
        assert_eq!(enabled["max_tokens"], 2048);
        let explicit = json!({"thinking":{"type":"disabled"},"enable_thinking":false,"reasoning_effort":"none"});
        assert_eq!(personai_generation_request_body(&explicit, false), explicit);
        assert_eq!(personai_generation_request_body(&explicit, true), explicit);
        for explicit in [
            json!({"reasoning_effort":"max"}),
            json!({"reasoning":{"effort":"high"}}),
            json!({"thinking":{"type":"enabled"}}),
            json!({"enable_thinking":true}),
        ] {
            assert_eq!(personai_generation_request_body(&explicit, false), explicit);
        }
    }
    #[test]
    fn native_request_preserves_order_pixels_text_and_dimensions() {
        let image = Content(vec![
            ContentBlock::Text {
                text: "diagram caption".into(),
            },
            ContentBlock::Media {
                media: MediaContent {
                    mime_type: "image/png".into(),
                    source: MediaSource::Base64("AA==".into()),
                    name: None,
                    estimated_tokens: Some(4096),
                },
            },
        ]);
        let mut request = EmbeddingRequest::texts(["正文"]);
        request.inputs.push(image);
        request.dimensions = Some(1024);
        let body = personai_embedding_request_body("actual-model", &request).unwrap();
        assert_eq!(body["model"], "actual-model");
        assert_eq!(body["dimensions"], 1024);
        assert_eq!(body["input"][0], "正文");
        assert_eq!(body["input"][1]["text"], "diagram caption");
        assert_eq!(body["input"][1]["image_base64"], "AA==");
    }
    #[test]
    fn sdk_embedding_keeps_text_and_image_together_and_rejects_unsupported_blocks() {
        let mut content = Content::from("caption");
        content.0.push(ContentBlock::Media {
            media: MediaContent {
                mime_type: "image/png".into(),
                source: MediaSource::Url("https://example.com/image.png".into()),
                name: None,
                estimated_tokens: Some(10),
            },
        });
        let input = serde_json::to_value(personai_embedding_input(&content).unwrap()).unwrap();
        assert_eq!(input["text"], "caption");
        assert_eq!(input["image_url"], "https://example.com/image.png");
        assert!(input["image_base64"].is_null());
        if let ContentBlock::Media { media } = &mut content.0[1] {
            media.source = MediaSource::Base64("AA==".into());
        }
        let input = serde_json::to_value(personai_embedding_input(&content).unwrap()).unwrap();
        assert_eq!(input["image_base64"], "AA==");
        assert!(input["image_url"].is_null());
        content.0.push(content.0[1].clone());
        assert!(personai_embedding_input(&content).is_err());
        assert!(
            personai_embedding_input(&Content(vec![ContentBlock::Json {
                value: json!({"x":1})
            }]))
            .is_err()
        );
    }
}
