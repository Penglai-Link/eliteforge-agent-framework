use crate::*;
use async_trait::async_trait;
use eliteforge_agent_plugin_core::ServiceRegistry;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, RwLock},
};

/// A complete replacement for one account's active model bindings. Every
/// adapter in this value must capture the same immutable authorization context.
#[derive(Clone)]
pub struct ModelRuntimeUpdate {
    pub account_scope: String,
    pub revision: u64,
    pub chat: Vec<ModelConnection>,
    pub embedding: Vec<EmbeddingModelService>,
    pub rerank: Vec<RerankModelService>,
    pub image: Vec<ImageGenerationModelService>,
    pub video: Vec<VideoGenerationModelService>,
    pub recognition: Vec<SpeechRecognitionModelService>,
    pub synthesis: Vec<SpeechSynthesisModelService>,
    pub realtime: Vec<RealtimeModelService>,
    pub detection: Vec<ContentDetectionModelService>,
    pub spaces: Vec<EmbeddingSpace>,
    pub routing: ModelRouting,
}
impl ModelRuntimeUpdate {
    pub fn new(
        account_scope: impl Into<String>,
        revision: u64,
        chat: Vec<ModelConnection>,
    ) -> Self {
        Self {
            account_scope: account_scope.into(),
            revision,
            chat,
            embedding: Vec::new(),
            rerank: Vec::new(),
            image: Vec::new(),
            video: Vec::new(),
            recognition: Vec::new(),
            synthesis: Vec::new(),
            realtime: Vec::new(),
            detection: Vec::new(),
            spaces: Vec::new(),
            routing: ModelRouting::default(),
        }
    }
}

pub struct ModelRuntimeSnapshot {
    pub account_scope: String,
    pub revision: u64,
    pub chat: Arc<ModelCatalog>,
    pub(crate) embedding: CapabilityCatalog<dyn EmbeddingModel>,
    pub(crate) rerank: CapabilityCatalog<dyn RerankModel>,
    pub(crate) image: CapabilityCatalog<dyn ImageGenerationModel>,
    pub(crate) video: CapabilityCatalog<dyn VideoGenerationModel>,
    pub(crate) recognition: CapabilityCatalog<dyn SpeechRecognitionModel>,
    pub(crate) synthesis: CapabilityCatalog<dyn SpeechSynthesisModel>,
    pub(crate) realtime: CapabilityCatalog<dyn RealtimeModel>,
    pub(crate) detection: CapabilityCatalog<dyn ContentDetectionModel>,
    pub(crate) spaces: BTreeMap<ModelRef, EmbeddingSpace>,
}
impl ModelRuntimeSnapshot {
    fn build(update: ModelRuntimeUpdate) -> Result<Self, AgentError> {
        let chat = Arc::new(ModelCatalog::new(update.chat)?);
        let embedding = CapabilityCatalog::new(update.embedding)?
            .with_routing(ModelOperation::Embedding, &update.routing)?;
        let mut spaces = BTreeMap::new();
        for space in update.spaces {
            if space.dimensions == 0
                || space.revision.trim().is_empty()
                || space.preprocessing.trim().is_empty()
                || embedding.get(&space.model).is_none()
                || spaces.insert(space.model.clone(), space).is_some()
            {
                return Err(AgentError::InvalidConfiguration(
                    "invalid, unregistered or duplicate embedding space".into(),
                ));
            }
        }
        macro_rules! catalog {
            ($field:ident, $operation:ident) => {
                CapabilityCatalog::new(update.$field)?
                    .with_routing(ModelOperation::$operation, &update.routing)?
            };
        }
        Ok(Self {
            account_scope: update.account_scope,
            revision: update.revision,
            chat,
            embedding,
            rerank: catalog!(rerank, Rerank),
            image: catalog!(image, ImageGeneration),
            video: catalog!(video, VideoGeneration),
            recognition: catalog!(recognition, SpeechRecognition),
            synthesis: catalog!(synthesis, SpeechSynthesis),
            realtime: catalog!(realtime, Realtime),
            detection: catalog!(detection, ContentDetection),
            spaces,
        })
    }
}

pub struct ModelRuntime {
    current: RwLock<Arc<ModelRuntimeSnapshot>>,
    partitions: Mutex<BTreeMap<String, ModelRuntimeUpdate>>,
    retired_video: Mutex<BTreeMap<(ModelRef, String), VideoGenerationModelService>>,
    retired_image: Mutex<BTreeMap<(ModelRef, String), ImageGenerationModelService>>,
    video_jobs: Mutex<BTreeMap<(ModelRef, String, String), VideoGenerationModelService>>,
    image_jobs: Mutex<BTreeMap<(ModelRef, String, String), ImageGenerationModelService>>,
}
impl ModelRuntime {
    pub fn new(update: ModelRuntimeUpdate) -> Result<Arc<Self>, AgentError> {
        let snapshot = Arc::new(ModelRuntimeSnapshot::build(update.clone())?);
        Ok(Arc::new(Self {
            current: RwLock::new(snapshot),
            partitions: Mutex::new(BTreeMap::from([("__base".into(), update)])),
            retired_video: Mutex::new(BTreeMap::new()),
            retired_image: Mutex::new(BTreeMap::new()),
            video_jobs: Mutex::new(BTreeMap::new()),
            image_jobs: Mutex::new(BTreeMap::new()),
        }))
    }
    pub fn snapshot(&self) -> Arc<ModelRuntimeSnapshot> {
        self.current
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
    pub fn replace(&self, update: ModelRuntimeUpdate) -> Result<(), AgentError> {
        let next = Arc::new(ModelRuntimeSnapshot::build(update.clone())?);
        let mut partitions = self
            .partitions
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut current = self
            .current
            .write()
            .unwrap_or_else(|error| error.into_inner());
        self.replace_locked(&mut current, next);
        partitions.clear();
        partitions.insert("__base".into(), update);
        Ok(())
    }
    /// Atomically replace only the provider connection owned by one integration.
    /// Other providers remain available in the same Agent.
    pub fn replace_connection(
        &self,
        connection_id: &str,
        update: ModelRuntimeUpdate,
    ) -> Result<(), AgentError> {
        self.replace_connection_inner(connection_id, None, update)
            .map(|_| ())
    }
    fn replace_base(&self, update: ModelRuntimeUpdate) -> Result<(), AgentError> {
        let mut partitions = self
            .partitions
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut next_partitions = partitions.clone();
        next_partitions.insert("__base".into(), update);
        let next = Arc::new(ModelRuntimeSnapshot::build(Self::combine(
            &next_partitions,
        ))?);
        let mut current = self
            .current
            .write()
            .unwrap_or_else(|error| error.into_inner());
        self.replace_locked(&mut current, next);
        *partitions = next_partitions;
        Ok(())
    }
    /// Fence a refresh against account changes that happened while it was in flight.
    pub fn replace_connection_if_current(
        &self,
        connection_id: &str,
        expected_scope: &str,
        expected_revision: u64,
        update: ModelRuntimeUpdate,
    ) -> Result<bool, AgentError> {
        self.replace_connection_inner(
            connection_id,
            Some((expected_scope, expected_revision)),
            update,
        )
    }
    fn replace_connection_inner(
        &self,
        connection_id: &str,
        expected: Option<(&str, u64)>,
        update: ModelRuntimeUpdate,
    ) -> Result<bool, AgentError> {
        if connection_id.trim().is_empty()
            || connection_id == "__base"
            || update.chat.iter().any(|item| item.id != connection_id)
            || update
                .embedding
                .iter()
                .any(|item| item.model.connection != connection_id)
            || update
                .rerank
                .iter()
                .any(|item| item.model.connection != connection_id)
            || update
                .image
                .iter()
                .any(|item| item.model.connection != connection_id)
            || update
                .video
                .iter()
                .any(|item| item.model.connection != connection_id)
            || update
                .recognition
                .iter()
                .any(|item| item.model.connection != connection_id)
            || update
                .synthesis
                .iter()
                .any(|item| item.model.connection != connection_id)
            || update
                .realtime
                .iter()
                .any(|item| item.model.connection != connection_id)
            || update
                .detection
                .iter()
                .any(|item| item.model.connection != connection_id)
        {
            return Err(AgentError::InvalidConfiguration(
                "model update contains another connection".into(),
            ));
        }
        let mut partitions = self
            .partitions
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some((scope, revision)) = expected {
            if update.account_scope != scope || update.revision < revision {
                return Err(AgentError::InvalidConfiguration(
                    "model refresh account scope or revision changed".into(),
                ));
            }
            let current = partitions.get(connection_id);
            if current
                .is_none_or(|value| value.account_scope != scope || value.revision != revision)
            {
                return Ok(false);
            }
        }
        let mut next_partitions = partitions.clone();
        next_partitions.insert(connection_id.into(), update);
        let next = Arc::new(ModelRuntimeSnapshot::build(Self::combine(
            &next_partitions,
        ))?);
        let mut current = self
            .current
            .write()
            .unwrap_or_else(|error| error.into_inner());
        self.replace_locked(&mut current, next);
        *partitions = next_partitions;
        Ok(true)
    }
    fn combine(partitions: &BTreeMap<String, ModelRuntimeUpdate>) -> ModelRuntimeUpdate {
        let mut combined = ModelRuntimeUpdate::new("composite", 0, Vec::new());
        for update in partitions.values() {
            combined.revision = combined.revision.max(update.revision);
            combined.chat.extend(update.chat.clone());
            combined.embedding.extend(update.embedding.clone());
            combined.rerank.extend(update.rerank.clone());
            combined.image.extend(update.image.clone());
            combined.video.extend(update.video.clone());
            combined.recognition.extend(update.recognition.clone());
            combined.synthesis.extend(update.synthesis.clone());
            combined.realtime.extend(update.realtime.clone());
            combined.detection.extend(update.detection.clone());
            combined.spaces.extend(update.spaces.clone());
            combined
                .routing
                .defaults
                .extend(update.routing.defaults.clone());
            combined
                .routing
                .stages
                .extend(update.routing.stages.clone());
        }
        combined
    }
    fn replace_locked(
        &self,
        current: &mut Arc<ModelRuntimeSnapshot>,
        next: Arc<ModelRuntimeSnapshot>,
    ) {
        let old = current.clone();
        let mut retired_video = self.retired_video.lock().unwrap_or_else(|e| e.into_inner());
        for model in old.video.models() {
            if next.video.get(model).is_none() {
                let binding = old.video.get(model).expect("listed binding");
                retired_video.insert((model.clone(), binding.protocol.clone()), binding.clone());
            }
        }
        let mut retired_image = self.retired_image.lock().unwrap_or_else(|e| e.into_inner());
        for model in old.image.models() {
            if next.image.get(model).is_none() {
                let binding = old.image.get(model).expect("listed binding");
                retired_image.insert((model.clone(), binding.protocol.clone()), binding.clone());
            }
        }
        *current = next;
    }
    pub(crate) fn video_job_binding(
        &self,
        model: &ModelRef,
        protocol: &str,
        job_id: &str,
    ) -> Option<VideoGenerationModelService> {
        if let Some(binding) = self
            .video_jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(model.clone(), protocol.to_owned(), job_id.to_owned()))
            .cloned()
        {
            return Some(binding);
        }
        self.snapshot()
            .video
            .get(model)
            .filter(|b| b.protocol == protocol)
            .cloned()
            .or_else(|| {
                self.retired_video
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&(model.clone(), protocol.to_owned()))
                    .cloned()
            })
    }
    pub(crate) fn image_job_binding(
        &self,
        model: &ModelRef,
        protocol: &str,
        job_id: &str,
    ) -> Option<ImageGenerationModelService> {
        if let Some(binding) = self
            .image_jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(model.clone(), protocol.to_owned(), job_id.to_owned()))
            .cloned()
        {
            return Some(binding);
        }
        self.snapshot()
            .image
            .get(model)
            .filter(|b| b.protocol == protocol)
            .cloned()
            .or_else(|| {
                self.retired_image
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&(model.clone(), protocol.to_owned()))
                    .cloned()
            })
    }
    pub(crate) fn retain_video_job(&self, job: &VideoJob, binding: VideoGenerationModelService) {
        self.video_jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                (job.model.clone(), job.protocol.clone(), job.id.clone()),
                binding,
            );
    }
    pub(crate) fn retain_image_job(&self, job: &ImageJob, binding: ImageGenerationModelService) {
        if !job.id.is_empty() {
            self.image_jobs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    (job.model.clone(), job.protocol.clone(), job.id.clone()),
                    binding,
                );
        }
    }
    pub(crate) fn from_registry(registry: &ServiceRegistry) -> Result<Arc<Self>, AgentError> {
        let mut update = ModelRuntimeUpdate::new(
            "",
            0,
            registry
                .all::<ModelConnectionService>()
                .iter()
                .map(|s| s.0.clone())
                .collect(),
        );
        macro_rules! collect {
            ($field:ident, $service:ty) => {
                update.$field = registry
                    .all::<$service>()
                    .iter()
                    .map(|s| (**s).clone())
                    .collect();
            };
        }
        collect!(embedding, EmbeddingModelService);
        collect!(rerank, RerankModelService);
        collect!(image, ImageGenerationModelService);
        collect!(video, VideoGenerationModelService);
        collect!(recognition, SpeechRecognitionModelService);
        collect!(synthesis, SpeechSynthesisModelService);
        collect!(realtime, RealtimeModelService);
        collect!(detection, ContentDetectionModelService);
        update.spaces = registry
            .all::<EmbeddingSpaceService>()
            .iter()
            .map(|s| s.0.clone())
            .collect();
        update.routing = registry
            .get::<ModelRouting>()
            .map(|routing| (*routing).clone())
            .unwrap_or_default();
        if let Some(existing) = registry.get::<ModelRuntimeService>() {
            existing.0.replace_base(update)?;
            return Ok(existing.0.clone());
        }
        Self::new(update)
    }
}

pub struct ModelRuntimeService(pub Arc<ModelRuntime>);
pub struct ModelRuntimePlugin(pub Arc<ModelRuntime>);
impl ModelRuntimePlugin {
    pub fn new(runtime: Arc<ModelRuntime>) -> Self {
        Self(runtime)
    }
}
#[async_trait]
impl Plugin for ModelRuntimePlugin {
    fn id(&self) -> &str {
        "framework.model-runtime"
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        context.provide(ModelRuntimeService(self.0.clone()))
    }
}
