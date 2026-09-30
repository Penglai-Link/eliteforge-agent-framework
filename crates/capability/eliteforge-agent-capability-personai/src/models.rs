//! Immutable model bindings capture one account context per catalog revision.
use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use base64::Engine;
use crab_serverapi::{
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
    ModelOperation, ModelRef, ModelRegistration, OperationCapabilities, RealtimeModel,
    ResourceReference, Role, SpeechRecognitionModel, SpeechRecognitionRequest,
    SpeechRecognitionResponse, ToolCall, Usage, VideoGenerationModel, VideoGenerationRequest,
    VideoJob,
};
use eliteforge_agent_framework::ModelRuntimeUpdate;
use serde_json::{Value, json};

use crate::realtime::PersonAiRealtimeAdapter;
use crate::sync::{DiscoveredChatModel, DiscoveredMediaModel, PersonAiDiscoverySnapshot};
use crate::{AccountContext, PersonAiConfig, PersonAiContextProvider, PersonAiServices};

/// Factory for immutable per-account bindings. It is also exposed as a typed service.
pub struct PersonAiModels {
    connection_id: String,
    services: Arc<PersonAiServices>,
    context: Arc<dyn PersonAiContextProvider>,
}

impl PersonAiModels {
    pub fn new(
        connection_id: String,
        services: Arc<PersonAiServices>,
        context: Arc<dyn PersonAiContextProvider>,
    ) -> Self {
        Self {
            connection_id,
            services,
            context,
        }
    }

    pub fn bind(&self, account: Arc<AccountContext>) -> Arc<BoundPersonAiModels> {
        Arc::new(BoundPersonAiModels {
            connection_id: self.connection_id.clone(),
            services: self.services.clone(),
            context: self.context.clone(),
            account,
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
            let default: Arc<dyn ChatModel> = Arc::new(PersonAiChatAdapter {
                binding: binding.clone(),
                protocol,
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
            let implementation: Arc<dyn EmbeddingModel> = binding.clone();
            update.embedding.push(
                ModelRegistration::<dyn EmbeddingModel>::new(binding.model_ref(id), implementation)
                    .protocol("personai.embeddings"),
            );
        }
        for (media_type, models) in &snapshot.media {
            for model in models {
                match *media_type {
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
                entries.push(ModelCatalogEntry {
                    operation,
                    model: ModelRef::new(&config.connection_id, &model.id),
                    protocol: "personai.multimedia".into(),
                    capabilities: OperationCapabilities::default(),
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

fn media_operation(media_type: i32) -> Option<ModelOperation> {
    match media_type {
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
    descriptor.context_window_tokens = model
        .context_length
        .and_then(|count| usize::try_from(count).ok());
    if model.image_understanding {
        descriptor.input_modalities.push(Modality::Image);
    }
    if model.video_understanding {
        descriptor.input_modalities.push(Modality::Video);
    }
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
    account: Arc<AccountContext>,
}

impl BoundPersonAiModels {
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
            Err(ModelError::Fatal(
                "PersonAI account context changed; select a model from the current catalog".into(),
            ))
        }
    }

    pub(crate) fn authorization(&self) -> &crab_serverapi::base::AuthorizedClientContext {
        &self.account.authorization
    }

    pub async fn chat_with_protocol(
        &self,
        model_id: &str,
        protocol: ProtocolType,
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
                    .create(context, &raw, Some(QualityType::Low), None)
                    .await
            }
            ProtocolType::Responses => self.services.responses.create(context, &raw).await,
        }
        .map_err(map_error)?;
        let parsed = match protocol {
            ProtocolType::ChatCompletions => parse_chat_response(response)?,
            ProtocolType::Responses => parse_responses_response(response)?,
        };
        if !parsed.content.is_empty() {
            stream.text_delta(&parsed.content.text()).await;
        }
        Ok(parsed)
    }
}

/// One protocol per adapter value ensures catalog bindings cannot silently change wire format.
pub struct PersonAiChatAdapter {
    pub binding: Arc<BoundPersonAiModels>,
    pub protocol: ProtocolType,
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
            .chat_with_protocol(model_id, self.protocol, request, stream)
            .await
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
        let inputs: Vec<String> = request
            .inputs
            .iter()
            .map(|content| {
                if content.has_media() {
                    Err(ModelError::Fatal(
                        "PersonAI embeddings accept text only".into(),
                    ))
                } else {
                    Ok(content.text())
                }
            })
            .collect::<Result<_, _>>()?;
        let mut body = json!({"model": model, "input": inputs});
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
            .map_err(map_error)?;
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
            model: self.model_ref(model),
            vectors,
            input_tokens: response["usage"]["prompt_tokens"].as_u64(),
        };
        result.validate(inputs.len(), request.dimensions)?;
        Ok(result)
    }
}

#[async_trait]
impl ImageGenerationModel for BoundPersonAiModels {
    async fn generate(
        &self,
        model: &str,
        request: ImageGenerationRequest,
    ) -> Result<ImageGenerationResponse, ModelError> {
        let job = self.submit(model, request).await?;
        if job.status != ModelJobStatus::Completed {
            return Err(ModelError::Fatal(
                "PersonAI image generation is asynchronous; use submit_image/query_image".into(),
            ));
        }
        Ok(ImageGenerationResponse {
            model: job.model,
            images: job.images,
        })
    }

    async fn submit(
        &self,
        model: &str,
        request: ImageGenerationRequest,
    ) -> Result<ImageJob, ModelError> {
        self.ensure_current()?;
        if !request.provider_options.is_empty() {
            return Err(ModelError::Fatal(
                "PersonAI SDK image generation has no provider_options field".into(),
            ));
        }
        let sdk_request = multimedia::ImageGenerationRequest {
            prompt: request.prompt,
            model_id: Some(model.into()),
            size: request.size,
            n: Some(request.count as i32),
            quality: request.quality,
        };
        let response = self
            .services
            .multimedia
            .generate_image(&self.account.authorization, &sdk_request)
            .await
            .map_err(map_error)?;
        Ok(self.image_job(model, response))
    }

    async fn query(&self, job: &ImageJob) -> Result<ImageJob, ModelError> {
        let response = self
            .services
            .multimedia
            .get_task(&self.account.authorization, &job.id)
            .await
            .map_err(map_error)?;
        Ok(self.image_job(&job.model.model, response))
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
        let sdk_request = multimedia::VideoGenerationRequest {
            prompt: request.prompt,
            model_id: Some(model.into()),
            duration: Some(request.seconds as i32),
            resolution: request.size,
            ..Default::default()
        };
        let response = self
            .services
            .multimedia
            .generate_video(&self.account.authorization, &sdk_request)
            .await
            .map_err(map_error)?;
        Ok(self.video_job(model, response))
    }

    async fn query(&self, job: &VideoJob) -> Result<VideoJob, ModelError> {
        let response = self
            .services
            .multimedia
            .get_task(&self.account.authorization, &job.id)
            .await
            .map_err(map_error)?;
        Ok(self.video_job(&job.model.model, response))
    }

    async fn cancel(&self, _job: &VideoJob) -> Result<(), ModelError> {
        Err(ModelError::Fatal(
            "PersonAI video task cancellation is unsupported by the SDK".into(),
        ))
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
                .map_err(map_error)?
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
                .map_err(map_error)?
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
            return Err(ModelError::Fatal(
                "PersonAI file transcription is server-selected; use speech-auto".into(),
            ));
        }
        let bytes = match request.audio.source {
            MediaSource::Base64(encoded) => base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|error| ModelError::Fatal(format!("invalid audio base64: {error}")))?,
            MediaSource::Url(_) => {
                return Err(ModelError::Fatal(
                    "PersonAI file transcription requires inline audio bytes".into(),
                ));
            }
        };
        let file_name = request.audio.name.unwrap_or_else(|| "audio.wav".into());
        let text = self
            .services
            .audio_file
            .transcribe(
                &self.account.authorization,
                &file_name,
                bytes,
                request.language.as_deref(),
            )
            .await
            .map_err(map_error)?;
        Ok(SpeechRecognitionResponse {
            model: self.model_ref(model),
            text,
            segments: Vec::new(),
        })
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
    match error {
        ServerApiError::Gateway {
            status: 502 | 503 | 504,
            ..
        } => ModelError::Transient(error.to_string()),
        ServerApiError::Http(_) | ServerApiError::WebSocket(_) => {
            ModelError::Transient(error.to_string())
        }
        _ => ModelError::Fatal(error.to_string()),
    }
}

fn chat_completions_request(model: &str, request: &ChatRequest) -> Result<Value, ModelError> {
    let messages = request.messages.iter().map(|message| {
        let role = match message.role {
            Role::System => "system", Role::Developer => "developer", Role::User => "user",
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
    apply_options(&mut body, request);
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(request.tools.iter().map(|tool| json!({
            "type": "function", "function": {"name": tool.call_name, "description": tool.description, "parameters": tool.input_schema}
        })).collect());
    }
    Ok(body)
}

fn responses_request(model: &str, request: &ChatRequest) -> Result<Value, ModelError> {
    let input = request
        .messages
        .iter()
        .map(|message| {
            let role = match message.role {
                Role::System | Role::Developer => "developer",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "user",
            };
            Ok(json!({"role": role, "content": content_for_chat(&message.content)?}))
        })
        .collect::<Result<Vec<_>, ModelError>>()?;
    let mut body = json!({"model": model, "input": input});
    apply_options(&mut body, request);
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(request.tools.iter().map(|tool| json!({
            "type": "function", "name": tool.call_name, "description": tool.description, "parameters": tool.input_schema
        })).collect());
    }
    Ok(body)
}

fn apply_options(body: &mut Value, request: &ChatRequest) {
    if let Some(value) = request.options.temperature {
        body["temperature"] = json!(value);
    }
    if let Some(value) = request.options.top_p {
        body["top_p"] = json!(value);
    }
    if let Some(value) = request.options.max_output_tokens {
        body["max_output_tokens"] = json!(value);
    }
    if let Some(value) = &request.options.stop_sequences {
        body["stop"] = json!(value);
    }
}

fn content_for_chat(content: &Content) -> Result<Value, ModelError> {
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
            } else if text.is_empty() {
                if let Some(content) = item["content"].as_array() {
                    for part in content {
                        if let Some(fragment) = part["text"].as_str() {
                            text.push_str(fragment);
                        }
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
