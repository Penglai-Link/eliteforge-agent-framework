//! Typed model operations. Modalities and protocols are independent of operations.
use crate::*;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    Text,
    Image,
    Audio,
    Video,
    Document,
}

impl Content {
    pub fn modalities(&self) -> Vec<Modality> {
        let mut result = Vec::new();
        for block in &self.0 {
            let kind = match block {
                ContentBlock::Text { .. } | ContentBlock::Json { .. } => Modality::Text,
                ContentBlock::Media { media } => modality_for_mime(&media.mime_type),
                // An opaque reference carries metadata, not the referenced media bytes.
                ContentBlock::Resource { .. } => Modality::Text,
            };
            if !result.contains(&kind) {
                result.push(kind);
            }
        }
        result
    }
}
fn modality_for_mime(mime: &str) -> Modality {
    if mime.starts_with("image/") {
        Modality::Image
    } else if mime.starts_with("audio/") {
        Modality::Audio
    } else if mime.starts_with("video/") {
        Modality::Video
    } else if mime.starts_with("text/") {
        Modality::Text
    } else {
        Modality::Document
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelOperation {
    Chat,
    Embedding,
    Rerank,
    ImageGeneration,
    VideoGeneration,
    SpeechRecognition,
    SpeechSynthesis,
    Realtime,
    ContentDetection,
}

#[derive(Clone, Default)]
pub struct ModelRouting {
    pub defaults: BTreeMap<ModelOperation, ModelRef>,
    pub stages: BTreeMap<(ModelOperation, String), ModelRef>,
}
impl ModelRouting {
    pub fn default_for(mut self, operation: ModelOperation, model: ModelRef) -> Self {
        self.defaults.insert(operation, model);
        self
    }
    pub fn stage_for(
        mut self,
        operation: ModelOperation,
        stage: impl Into<String>,
        model: ModelRef,
    ) -> Self {
        self.stages.insert((operation, stage.into()), model);
        self
    }
    pub fn selected(&self, operation: ModelOperation, stage: &str) -> Option<&ModelRef> {
        self.stages
            .get(&(operation, stage.into()))
            .or_else(|| self.defaults.get(&operation))
    }
}
pub struct ModelRoutingPlugin(pub ModelRouting);
#[async_trait]
impl Plugin for ModelRoutingPlugin {
    fn id(&self) -> &str {
        "model.routing"
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        context.provide(self.0.clone())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationCapabilities {
    pub input: Vec<Modality>,
    pub output: Vec<Modality>,
    pub streaming: bool,
    pub cancellation: bool,
}
impl Default for OperationCapabilities {
    fn default() -> Self {
        Self {
            input: vec![Modality::Text],
            output: vec![Modality::Text],
            streaming: false,
            cancellation: false,
        }
    }
}
impl OperationCapabilities {
    pub fn accepts(&self, inputs: &[Content]) -> bool {
        inputs
            .iter()
            .flat_map(Content::modalities)
            .all(|m| self.input.contains(&m))
    }
}

/// Selection is scoped to one operation. Explicit choices never silently fall back.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelChoice {
    pub model: Option<ModelRef>,
    pub stage: String,
}
impl ModelChoice {
    pub fn model(model: ModelRef) -> Self {
        Self {
            model: Some(model),
            stage: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingRequest {
    pub choice: ModelChoice,
    pub inputs: Vec<Content>,
    pub dimensions: Option<usize>,
    pub purpose: EmbeddingPurpose,
}
impl EmbeddingRequest {
    pub fn texts(inputs: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            choice: ModelChoice::default(),
            inputs: inputs
                .into_iter()
                .map(|s| Content::from(s.into()))
                .collect(),
            dimensions: None,
            purpose: EmbeddingPurpose::Document,
        }
    }
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum EmbeddingPurpose {
    Query,
    #[default]
    Document,
    Similarity,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingVector {
    pub index: usize,
    pub values: Vec<f32>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingResponse {
    pub model: ModelRef,
    pub vectors: Vec<EmbeddingVector>,
    /// Missing usage remains unknown.
    pub input_tokens: Option<u64>,
}
impl EmbeddingResponse {
    pub fn validate(&self, count: usize, dimensions: Option<usize>) -> Result<(), ModelError> {
        if count == 0 || self.vectors.len() != count {
            return Err(ModelError::Fatal(
                "embedding count does not match input".into(),
            ));
        }
        let mut seen = vec![false; count];
        let expected = dimensions.unwrap_or(self.vectors[0].values.len());
        for vector in &self.vectors {
            if vector.index >= count
                || seen[vector.index]
                || expected == 0
                || vector.values.len() != expected
                || vector.values.iter().any(|v| !v.is_finite())
            {
                return Err(ModelError::Fatal(
                    "invalid embedding index, dimension or value".into(),
                ));
            }
            seen[vector.index] = true;
        }
        Ok(())
    }
}
/// Persist alongside the vector index; query and document purposes may differ.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingSpace {
    pub model: ModelRef,
    pub revision: String,
    pub dimensions: usize,
    pub preprocessing: String,
}
impl EmbeddingSpace {
    pub fn bind_query(&self, request: &mut EmbeddingRequest) -> Result<(), ModelError> {
        if self.dimensions == 0
            || self.revision.is_empty()
            || self.preprocessing.is_empty()
            || request
                .choice
                .model
                .as_ref()
                .is_some_and(|m| m != &self.model)
            || request.dimensions.is_some_and(|d| d != self.dimensions)
        {
            return Err(ModelError::Fatal(
                "embedding request conflicts with index space".into(),
            ));
        }
        request.choice.model = Some(self.model.clone());
        request.dimensions = Some(self.dimensions);
        request.purpose = EmbeddingPurpose::Query;
        Ok(())
    }
}
#[crate::extension_point]
#[async_trait]
pub trait EmbeddingModel: Send + Sync {
    async fn embed(
        &self,
        model: &str,
        request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, ModelError>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RerankRequest {
    pub choice: ModelChoice,
    pub query: Content,
    pub documents: Vec<Content>,
    pub top_n: Option<usize>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankedDocument {
    pub index: usize,
    pub score: f64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RerankResponse {
    pub model: ModelRef,
    pub results: Vec<RankedDocument>,
}
#[crate::extension_point]
#[async_trait]
pub trait RerankModel: Send + Sync {
    async fn rerank(
        &self,
        model: &str,
        request: RerankRequest,
    ) -> Result<RerankResponse, ModelError>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageGenerationRequest {
    pub choice: ModelChoice,
    pub prompt: String,
    pub size: Option<String>,
    pub count: u32,
    #[serde(default)]
    pub quality: Option<String>,
    #[serde(default)]
    pub provider_options: BTreeMap<String, serde_json::Value>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageGenerationResponse {
    pub model: ModelRef,
    pub images: Vec<MediaContent>,
}
#[crate::extension_point]
#[async_trait]
pub trait ImageGenerationModel: Send + Sync {
    async fn generate(
        &self,
        model: &str,
        request: ImageGenerationRequest,
    ) -> Result<ImageGenerationResponse, ModelError>;
    async fn submit(
        &self,
        model: &str,
        request: ImageGenerationRequest,
    ) -> Result<ImageJob, ModelError> {
        let response = self.generate(model, request).await?;
        Ok(ImageJob {
            model: response.model,
            protocol: "custom".into(),
            id: String::new(),
            status: ModelJobStatus::Completed,
            progress: Some(1.0),
            images: response.images,
            error: None,
        })
    }
    async fn query(&self, job: &ImageJob) -> Result<ImageJob, ModelError> {
        if job.status == ModelJobStatus::Completed {
            Ok(job.clone())
        } else {
            Err(ModelError::Fatal("image job querying is unsupported".into()))
        }
    }
    async fn cancel(&self, _job: &ImageJob) -> Result<(), ModelError> {
        Err(ModelError::Fatal("image cancellation is unsupported".into()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageJob {
    pub model: ModelRef,
    pub protocol: String,
    pub id: String,
    pub status: ModelJobStatus,
    pub progress: Option<f64>,
    pub images: Vec<MediaContent>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoGenerationRequest {
    pub choice: ModelChoice,
    pub prompt: String,
    pub seconds: u32,
    pub size: Option<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelJobStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentDetectionRequest {
    pub choice: ModelChoice,
    pub content: Content,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentDetectionResponse {
    pub model: ModelRef,
    pub result: serde_json::Value,
}
#[crate::extension_point]
#[async_trait]
pub trait ContentDetectionModel: Send + Sync {
    async fn detect(
        &self,
        model: &str,
        request: ContentDetectionRequest,
    ) -> Result<ContentDetectionResponse, ModelError>;
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoJob {
    pub model: ModelRef,
    pub protocol: String,
    pub id: String,
    pub status: ModelJobStatus,
    pub progress: Option<f64>,
    pub output: Option<ResourceReference>,
    pub error: Option<String>,
}
#[crate::extension_point]
#[async_trait]
pub trait VideoGenerationModel: Send + Sync {
    async fn submit(
        &self,
        model: &str,
        request: VideoGenerationRequest,
    ) -> Result<VideoJob, ModelError>;
    async fn query(&self, job: &VideoJob) -> Result<VideoJob, ModelError>;
    async fn cancel(&self, _job: &VideoJob) -> Result<(), ModelError> {
        Err(ModelError::Fatal(
            "video cancellation is unsupported".into(),
        ))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeechRecognitionRequest {
    pub choice: ModelChoice,
    pub audio: MediaContent,
    pub language: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptSegment {
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub text: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeechRecognitionResponse {
    pub model: ModelRef,
    pub text: String,
    pub segments: Vec<TranscriptSegment>,
}
#[crate::extension_point]
#[async_trait]
pub trait SpeechRecognitionModel: Send + Sync {
    async fn transcribe(
        &self,
        model: &str,
        request: SpeechRecognitionRequest,
    ) -> Result<SpeechRecognitionResponse, ModelError>;
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeechSynthesisRequest {
    pub choice: ModelChoice,
    pub text: String,
    pub voice: String,
    pub format: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeechSynthesisResponse {
    pub model: ModelRef,
    pub audio: MediaContent,
}
#[crate::extension_point]
#[async_trait]
pub trait SpeechSynthesisModel: Send + Sync {
    async fn synthesize(
        &self,
        model: &str,
        request: SpeechSynthesisRequest,
    ) -> Result<SpeechSynthesisResponse, ModelError>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RealtimeRequest {
    pub choice: ModelChoice,
    pub input: Vec<Modality>,
    pub output: Vec<Modality>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RealtimeInput {
    Content(Content),
    Commit,
    Interrupt,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RealtimeEvent {
    Content(Content),
    Transcript(String),
    Completed,
    Error(String),
}
#[async_trait]
pub trait RealtimeSession: Send + Sync {
    async fn send(&self, input: RealtimeInput) -> Result<(), ModelError>;
    async fn next(&self) -> Result<Option<RealtimeEvent>, ModelError>;
    async fn close(&self) -> Result<(), ModelError>;
}
#[crate::extension_point]
#[async_trait]
pub trait RealtimeModel: Send + Sync {
    /// The adapter must release its transport when cancellation fires or the session is dropped.
    async fn connect(
        &self,
        model: &str,
        request: RealtimeRequest,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<Arc<dyn RealtimeSession>, ModelError>;
}
pub type RealtimeModelService = ModelRegistration<dyn RealtimeModel>;

/// Register this alongside an embedding binding when the model backs a persistent index.
#[derive(Clone)]
pub struct EmbeddingSpaceService(pub EmbeddingSpace);

#[derive(Clone)]
pub struct ModelInvocationOptions {
    pub timeout: std::time::Duration,
    pub cancellation: tokio_util::sync::CancellationToken,
    pub max_input_bytes: usize,
    pub max_batch_size: usize,
    /// Shares cancellation and token accounting with an Agent run when present.
    pub run: Option<RunContext>,
}
impl Default for ModelInvocationOptions {
    fn default() -> Self {
        Self {
            timeout: std::time::Duration::from_secs(60),
            cancellation: tokio_util::sync::CancellationToken::new(),
            max_input_bytes: 16 * 1024 * 1024,
            max_batch_size: 1024,
            run: None,
        }
    }
}
impl ModelInvocationOptions {
    pub fn for_run(run: &RunContext, limits: &RunLimits) -> Self {
        let mut bounded = run.clone();
        bounded.token_ceiling = bounded.token_ceiling.min(limits.max_total_tokens);
        Self {
            run: Some(bounded),
            timeout: limits.model_timeout,
            cancellation: run.cancellation.child_token(),
            ..Self::default()
        }
    }
}

/// Operation-specific execution through the registered catalog, usable by apps and workflow nodes.
#[async_trait]
pub trait ModelOperations: Send + Sync {
    #[doc(hidden)]
    fn stop(&self);
    #[doc(hidden)]
    async fn drain(&self);
    fn with_options(&self, options: ModelInvocationOptions) -> Arc<dyn ModelOperations>;
    fn models(&self, operation: ModelOperation) -> Vec<ModelRef>;
    async fn embed(&self, request: EmbeddingRequest) -> Result<EmbeddingResponse, AgentError>;
    async fn embed_query(
        &self,
        space: &EmbeddingSpace,
        request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, AgentError>;
    async fn rerank(&self, request: RerankRequest) -> Result<RerankResponse, AgentError>;
    async fn generate_image(
        &self,
        request: ImageGenerationRequest,
    ) -> Result<ImageGenerationResponse, AgentError>;
    async fn submit_image(&self, request: ImageGenerationRequest) -> Result<ImageJob, AgentError>;
    async fn query_image(&self, job: &ImageJob) -> Result<ImageJob, AgentError>;
    async fn cancel_image(&self, job: &ImageJob) -> Result<(), AgentError>;
    async fn submit_video(&self, request: VideoGenerationRequest) -> Result<VideoJob, AgentError>;
    async fn query_video(&self, job: &VideoJob) -> Result<VideoJob, AgentError>;
    async fn cancel_video(&self, job: &VideoJob) -> Result<(), AgentError>;
    async fn transcribe(
        &self,
        request: SpeechRecognitionRequest,
    ) -> Result<SpeechRecognitionResponse, AgentError>;
    async fn synthesize(
        &self,
        request: SpeechSynthesisRequest,
    ) -> Result<SpeechSynthesisResponse, AgentError>;
    async fn connect_realtime(
        &self,
        request: RealtimeRequest,
    ) -> Result<Arc<dyn RealtimeSession>, AgentError>;
    async fn detect_content(
        &self,
        request: ContentDetectionRequest,
    ) -> Result<ContentDetectionResponse, AgentError>;
}

/// An individual model/operation binding. Protocol names remain extensible.
pub struct ModelRegistration<T: ?Sized> {
    pub model: ModelRef,
    pub protocol: String,
    pub capabilities: OperationCapabilities,
    pub implementation: Arc<T>,
}
impl<T: ?Sized> Clone for ModelRegistration<T> {
    fn clone(&self) -> Self {
        Self {
            model: self.model.clone(),
            protocol: self.protocol.clone(),
            capabilities: self.capabilities.clone(),
            implementation: self.implementation.clone(),
        }
    }
}
impl<T: ?Sized> ModelRegistration<T> {
    pub fn protocol(mut self, protocol: impl Into<String>) -> Self {
        self.protocol = protocol.into();
        self
    }
    pub fn capabilities(mut self, capabilities: OperationCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }
}
macro_rules! registration {
    ($contract:ident, [$($input:ident),*], [$($output:ident),*]) => {
        impl ModelRegistration<dyn $contract> {
            pub fn new(model: ModelRef, implementation: Arc<dyn $contract>) -> Self {
                Self { model, implementation, protocol: "custom".into(), capabilities: OperationCapabilities {
                    input: vec![$(Modality::$input),*], output: vec![$(Modality::$output),*], ..Default::default()
                } }
            }
        }
    };
}
registration!(EmbeddingModel, [Text], []);
registration!(RerankModel, [Text], []);
registration!(ImageGenerationModel, [Text], [Image]);
registration!(VideoGenerationModel, [Text], [Video]);
registration!(SpeechRecognitionModel, [Audio], [Text]);
registration!(SpeechSynthesisModel, [Text], [Audio]);
registration!(RealtimeModel, [Text, Audio], [Text, Audio]);
registration!(ContentDetectionModel, [Text, Image], [Text]);
pub type EmbeddingModelService = ModelRegistration<dyn EmbeddingModel>;
pub type RerankModelService = ModelRegistration<dyn RerankModel>;
pub type ImageGenerationModelService = ModelRegistration<dyn ImageGenerationModel>;
pub type VideoGenerationModelService = ModelRegistration<dyn VideoGenerationModel>;
pub type SpeechRecognitionModelService = ModelRegistration<dyn SpeechRecognitionModel>;
pub type SpeechSynthesisModelService = ModelRegistration<dyn SpeechSynthesisModel>;
pub type ContentDetectionModelService = ModelRegistration<dyn ContentDetectionModel>;

macro_rules! model_registration_plugin {
    ($name:ident, $service:ty, $contract:ident, $prefix:literal) => {
        /// Registers one explicitly named model-operation binding.
        pub struct $name {
            id: String,
            registration: $service,
        }

        impl $name {
            pub fn new(registration: $service) -> Self {
                let id = format!(
                    concat!($prefix, ".{}.{}"),
                    registration.model.connection, registration.model.model
                );
                Self { id, registration }
            }

            pub fn with_id(id: impl Into<String>, registration: $service) -> Self {
                Self {
                    id: id.into(),
                    registration,
                }
            }
        }

        #[async_trait]
        impl Plugin for $name {
            fn id(&self) -> &str {
                &self.id
            }

            async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
                context.contribute(self.registration.clone())?;
                context
                    .contribute_extension::<dyn $contract>(self.registration.implementation.clone())
            }
        }
    };
}

model_registration_plugin!(
    EmbeddingModelPlugin,
    EmbeddingModelService,
    EmbeddingModel,
    "model.embedding"
);
model_registration_plugin!(
    RerankModelPlugin,
    RerankModelService,
    RerankModel,
    "model.rerank"
);
model_registration_plugin!(
    ImageGenerationModelPlugin,
    ImageGenerationModelService,
    ImageGenerationModel,
    "model.image-generation"
);
model_registration_plugin!(
    VideoGenerationModelPlugin,
    VideoGenerationModelService,
    VideoGenerationModel,
    "model.video-generation"
);
model_registration_plugin!(
    SpeechRecognitionModelPlugin,
    SpeechRecognitionModelService,
    SpeechRecognitionModel,
    "model.speech-recognition"
);
model_registration_plugin!(
    SpeechSynthesisModelPlugin,
    SpeechSynthesisModelService,
    SpeechSynthesisModel,
    "model.speech-synthesis"
);
model_registration_plugin!(
    RealtimeModelPlugin,
    RealtimeModelService,
    RealtimeModel,
    "model.realtime"
);
model_registration_plugin!(
    ContentDetectionModelPlugin,
    ContentDetectionModelService,
    ContentDetectionModel,
    "model.content-detection"
);

pub struct CapabilityCatalog<T: ?Sized> {
    bindings: BTreeMap<ModelRef, ModelRegistration<T>>,
    default: Option<ModelRef>,
    stages: BTreeMap<String, ModelRef>,
}
impl<T: ?Sized> Clone for CapabilityCatalog<T> {
    fn clone(&self) -> Self {
        Self {
            bindings: self.bindings.clone(),
            default: self.default.clone(),
            stages: self.stages.clone(),
        }
    }
}
impl<T: ?Sized> CapabilityCatalog<T> {
    pub fn new(bindings: Vec<ModelRegistration<T>>) -> Result<Self, AgentError> {
        let mut catalog = Self {
            bindings: BTreeMap::new(),
            default: None,
            stages: BTreeMap::new(),
        };
        for binding in bindings {
            if binding.model.connection.trim().is_empty()
                || binding.model.model.trim().is_empty()
                || binding.protocol.trim().is_empty()
                || catalog
                    .bindings
                    .insert(binding.model.clone(), binding)
                    .is_some()
            {
                return Err(AgentError::InvalidConfiguration(
                    "invalid or duplicate model operation binding".into(),
                ));
            }
        }
        Ok(catalog)
    }
    pub fn with_default(mut self, model: Option<ModelRef>) -> Result<Self, AgentError> {
        if model
            .as_ref()
            .is_some_and(|m| !self.bindings.contains_key(m))
        {
            return Err(AgentError::InvalidConfiguration(
                "default model is not registered".into(),
            ));
        }
        self.default = model;
        Ok(self)
    }
    pub fn with_stage(
        mut self,
        stage: impl Into<String>,
        model: ModelRef,
    ) -> Result<Self, AgentError> {
        if !self.bindings.contains_key(&model) {
            return Err(AgentError::InvalidConfiguration(
                "stage model is not registered".into(),
            ));
        }
        self.stages.insert(stage.into(), model);
        Ok(self)
    }
    pub fn models(&self) -> impl Iterator<Item = &ModelRef> {
        self.bindings.keys()
    }
    pub fn get(&self, model: &ModelRef) -> Option<&ModelRegistration<T>> {
        self.bindings.get(model)
    }
    pub fn with_routing(
        mut self,
        operation: ModelOperation,
        routing: &ModelRouting,
    ) -> Result<Self, AgentError> {
        self = self.with_default(routing.defaults.get(&operation).cloned())?;
        for ((op, stage), model) in &routing.stages {
            if *op == operation {
                self = self.with_stage(stage, model.clone())?;
            }
        }
        Ok(self)
    }
    pub fn select(
        &self,
        choice: &ModelChoice,
        inputs: &[Content],
    ) -> Result<&ModelRegistration<T>, AgentError> {
        let modalities: Vec<_> = inputs.iter().flat_map(Content::modalities).collect();
        self.select_modalities(choice, &modalities, &[])
    }
    pub fn select_modalities(
        &self,
        choice: &ModelChoice,
        input: &[Modality],
        output: &[Modality],
    ) -> Result<&ModelRegistration<T>, AgentError> {
        let accepts = |b: &&ModelRegistration<T>| {
            input.iter().all(|m| b.capabilities.input.contains(m))
                && output.iter().all(|m| b.capabilities.output.contains(m))
        };
        if let Some(model) = choice
            .model
            .as_ref()
            .or_else(|| self.stages.get(&choice.stage))
            .or(self.default.as_ref())
        {
            let binding = self.bindings.get(model).ok_or_else(|| {
                AgentError::InvalidConfiguration(
                    "selected model is not registered for this operation".into(),
                )
            })?;
            if !accepts(&binding) {
                return Err(AgentError::InvalidConfiguration(
                    "selected model does not support the requested modalities".into(),
                ));
            }
            return Ok(binding);
        }
        let mut candidates = self.bindings.values().filter(accepts);
        let selected = candidates.next().ok_or_else(|| {
            AgentError::InvalidConfiguration("no compatible model for this operation".into())
        })?;
        if candidates.next().is_some() {
            return Err(AgentError::InvalidConfiguration(
                "multiple compatible models require an explicit choice, stage rule or default"
                    .into(),
            ));
        }
        Ok(selected)
    }
}
