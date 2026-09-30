use crate::*;
use async_trait::async_trait;
use std::{future::Future, sync::Arc};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

struct Catalogs {
    runtime: Arc<ModelRuntime>,
    stopped: CancellationToken,
    active: RwLock<()>,
}
impl Drop for Catalogs {
    fn drop(&mut self) {
        self.stopped.cancel();
    }
}

/// Executes typed operations with deterministic selection and bounded, cancellable dispatch.
#[derive(Clone)]
pub struct ModelClient {
    catalogs: Arc<Catalogs>,
    options: ModelInvocationOptions,
}
impl ModelClient {
    pub(crate) fn from_runtime(runtime: Arc<ModelRuntime>) -> Self {
        Self {
            catalogs: Arc::new(Catalogs {
                runtime,
                stopped: CancellationToken::new(),
                active: RwLock::new(()),
            }),
            options: ModelInvocationOptions::default(),
        }
    }
    fn check_inputs(&self, inputs: &[Content]) -> Result<(), AgentError> {
        if inputs.is_empty()
            || inputs.len() > self.options.max_batch_size
            || self.options.max_input_bytes == 0
        {
            return Err(invalid("invalid model input batch size"));
        }
        let mut bytes = 0usize;
        for content in inputs {
            if content.is_empty() {
                return Err(invalid("model input must not be empty"));
            }
            bytes = bytes
                .checked_add(
                    serde_json::to_vec(content)
                        .map_err(|_| invalid("invalid model input"))?
                        .len(),
                )
                .ok_or_else(|| invalid("model input size overflow"))?;
            if bytes > self.options.max_input_bytes {
                return Err(invalid("model input exceeds byte limit"));
            }
        }
        Ok(())
    }
    async fn dispatch<T>(
        &self,
        operation: ModelOperation,
        model: &ModelRef,
        future: impl Future<Output = Result<T, ModelError>>,
    ) -> Result<T, AgentError> {
        if self.options.timeout.is_zero() {
            return Err(invalid("model timeout must be positive"));
        }
        let _active = self.catalogs.active.read().await;
        if self.catalogs.stopped.is_cancelled() {
            return Err(AgentError::Closed);
        }
        if let Some(run) = &self.options.run {
            run.check_cancelled()?;
            run.emit(AgentEvent::ModelOperationStarted {
                operation,
                model: model.clone(),
            })
            .await;
        }
        tokio::select! {
            biased;
            _ = self.catalogs.stopped.cancelled() => Err(AgentError::Closed),
            _ = self.options.cancellation.cancelled() => Err(AgentError::Cancelled),
            result = tokio::time::timeout(self.options.timeout, future) => {
                result.map_err(|_| ModelError::Transient("model operation timed out".into()))?.map_err(Into::into)
            }
        }
    }
    fn video_binding(&self, job: &VideoJob) -> Result<VideoGenerationModelService, AgentError> {
        let binding = self
            .catalogs
            .runtime
            .video_job_binding(job)
            .ok_or_else(|| {
                ModelError::Unavailable("video job binding is unavailable in this process".into())
            })?;
        if job.id.trim().is_empty() || binding.protocol != job.protocol {
            return Err(invalid(
                "video job protocol does not match its model binding",
            ));
        }
        Ok(binding)
    }
}

#[async_trait]
impl ModelOperations for ModelClient {
    fn stop(&self) {
        self.catalogs.stopped.cancel();
    }
    async fn drain(&self) {
        drop(self.catalogs.active.write().await);
    }
    fn with_options(&self, options: ModelInvocationOptions) -> Arc<dyn ModelOperations> {
        Arc::new(Self {
            catalogs: self.catalogs.clone(),
            options,
        })
    }
    fn models(&self, operation: ModelOperation) -> Vec<ModelRef> {
        let snapshot = self.catalogs.runtime.snapshot();
        match operation {
            ModelOperation::Embedding => snapshot.embedding.models().cloned().collect(),
            ModelOperation::Rerank => snapshot.rerank.models().cloned().collect(),
            ModelOperation::ImageGeneration => snapshot.image.models().cloned().collect(),
            ModelOperation::VideoGeneration => snapshot.video.models().cloned().collect(),
            ModelOperation::SpeechRecognition => snapshot.recognition.models().cloned().collect(),
            ModelOperation::SpeechSynthesis => snapshot.synthesis.models().cloned().collect(),
            ModelOperation::Realtime => snapshot.realtime.models().cloned().collect(),
            ModelOperation::ContentDetection => snapshot.detection.models().cloned().collect(),
            ModelOperation::Chat => snapshot.chat.models(),
        }
    }
    async fn embed(&self, mut request: EmbeddingRequest) -> Result<EmbeddingResponse, AgentError> {
        let snapshot = self.catalogs.runtime.snapshot();
        self.check_inputs(&request.inputs)?;
        if request.dimensions == Some(0) {
            return Err(invalid("embedding dimensions must be positive"));
        }
        let binding = snapshot
            .embedding
            .select(&request.choice, &request.inputs)?;
        if let Some(space) = snapshot.spaces.get(&binding.model) {
            if request.dimensions.is_some_and(|d| d != space.dimensions) {
                return Err(invalid(
                    "embedding dimensions conflict with registered space",
                ));
            }
            request.dimensions = Some(space.dimensions);
        }
        let count = request.inputs.len();
        let dimensions = request.dimensions;
        let estimate = request
            .inputs
            .iter()
            .map(|c| c.estimated_tokens() as u64)
            .fold(0u64, u64::saturating_add)
            .max(1);
        request.choice.model = Some(binding.model.clone());
        let mut permit = match &self.options.run {
            Some(run) => {
                let (permit, _) = crate::budget::UsagePermit::reserve(
                    run.usage.clone(),
                    estimate,
                    0,
                    run.token_ceiling,
                )?;
                run.persist_budget().await?;
                Some(permit)
            }
            None => None,
        };
        let result = self
            .dispatch(
                ModelOperation::Embedding,
                &binding.model,
                binding.implementation.embed(&binding.model.model, request),
            )
            .await;
        let result = (|| -> Result<_, AgentError> {
            match result {
                Ok(mut response) => {
                    if let (Some(permit), Some(run)) = (&mut permit, &self.options.run) {
                        permit.settle(
                            &Usage {
                                input_tokens: response.input_tokens.unwrap_or(estimate),
                                output_tokens: 0,
                            },
                            run.token_ceiling,
                        )?;
                    }
                    if response.model != binding.model {
                        return Err(invalid("embedding response model does not match binding"));
                    }
                    response.validate(count, dimensions)?;
                    response.vectors.sort_by_key(|v| v.index);
                    Ok(response)
                }
                Err(error) => Err(error),
            }
        })();
        drop(permit);
        if let Some(run) = &self.options.run {
            run.persist_budget().await?;
        }
        result
    }
    async fn embed_query(
        &self,
        space: &EmbeddingSpace,
        mut request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, AgentError> {
        if self.catalogs.runtime.snapshot().spaces.get(&space.model) != Some(space) {
            return Err(invalid(
                "query embedding space does not match registered model, revision, dimensions and preprocessing",
            ));
        }
        space.bind_query(&mut request)?;
        self.embed(request).await
    }
    async fn rerank(&self, mut request: RerankRequest) -> Result<RerankResponse, AgentError> {
        let snapshot = self.catalogs.runtime.snapshot();
        let mut inputs = request.documents.clone();
        inputs.push(request.query.clone());
        self.check_inputs(&inputs)?;
        let count = request.documents.len();
        if count == 0 || request.top_n.is_some_and(|n| n == 0 || n > count) {
            return Err(invalid("invalid rerank top_n or document count"));
        }
        let max = request.top_n.unwrap_or(count);
        let binding = snapshot.rerank.select(&request.choice, &inputs)?;
        request.choice.model = Some(binding.model.clone());
        let response = self
            .dispatch(
                ModelOperation::Rerank,
                &binding.model,
                binding.implementation.rerank(&binding.model.model, request),
            )
            .await?;
        let mut seen = std::collections::BTreeSet::new();
        if response.model != binding.model
            || response.results.len() > max
            || response
                .results
                .iter()
                .any(|r| r.index >= count || !r.score.is_finite() || !seen.insert(r.index))
        {
            return Err(invalid("invalid rerank response"));
        }
        Ok(response)
    }
    async fn generate_image(
        &self,
        mut request: ImageGenerationRequest,
    ) -> Result<ImageGenerationResponse, AgentError> {
        let snapshot = self.catalogs.runtime.snapshot();
        let inputs = [Content::from(request.prompt.clone())];
        self.check_inputs(&inputs)?;
        if request.count == 0 || request.count as usize > self.options.max_batch_size {
            return Err(invalid("invalid image count"));
        }
        let count = request.count as usize;
        let binding = snapshot.image.select(&request.choice, &inputs)?;
        request.choice.model = Some(binding.model.clone());
        let response = self
            .dispatch(
                ModelOperation::ImageGeneration,
                &binding.model,
                binding
                    .implementation
                    .generate(&binding.model.model, request),
            )
            .await?;
        if response.model != binding.model
            || response.images.len() != count
            || response
                .images
                .iter()
                .any(|m| !m.mime_type.starts_with("image/"))
        {
            return Err(invalid("invalid image generation response"));
        }
        Ok(response)
    }
    async fn submit_image(
        &self,
        mut request: ImageGenerationRequest,
    ) -> Result<ImageJob, AgentError> {
        let snapshot = self.catalogs.runtime.snapshot();
        let inputs = [Content::from(request.prompt.clone())];
        self.check_inputs(&inputs)?;
        if request.count == 0 || request.count as usize > self.options.max_batch_size {
            return Err(invalid("invalid image count"));
        }
        let binding = snapshot.image.select(&request.choice, &inputs)?;
        request.choice.model = Some(binding.model.clone());
        let mut job = self
            .dispatch(
                ModelOperation::ImageGeneration,
                &binding.model,
                binding.implementation.submit(&binding.model.model, request),
            )
            .await?;
        if job.model != binding.model || (!job.id.is_empty() && job.protocol != binding.protocol) {
            return Err(invalid("invalid image job binding"));
        }
        if job.id.is_empty() {
            job.protocol = binding.protocol.clone();
        }
        job.binding_id = Some(RunId::new_v4());
        self.catalogs
            .runtime
            .retain_image_job(&job, binding.clone());
        Ok(job)
    }
    async fn query_image(&self, job: &ImageJob) -> Result<ImageJob, AgentError> {
        let binding = self
            .catalogs
            .runtime
            .image_job_binding(job)
            .ok_or_else(|| {
                ModelError::Unavailable("image job binding is unavailable in this process".into())
            })?;
        let mut next = self
            .dispatch(
                ModelOperation::ImageGeneration,
                &binding.model,
                binding.implementation.query(job),
            )
            .await?;
        if next.id != job.id || next.model != job.model || next.protocol != job.protocol {
            return Err(invalid("image query changed job identity"));
        }
        next.binding_id = job.binding_id;
        Ok(next)
    }
    async fn cancel_image(&self, job: &ImageJob) -> Result<(), AgentError> {
        let binding = self
            .catalogs
            .runtime
            .image_job_binding(job)
            .ok_or_else(|| {
                ModelError::Unavailable("image job binding is unavailable in this process".into())
            })?;
        if !binding.capabilities.cancellation {
            return Err(invalid("image binding does not support cancellation"));
        }
        self.dispatch(
            ModelOperation::ImageGeneration,
            &binding.model,
            binding.implementation.cancel(job),
        )
        .await
    }
    async fn submit_video(
        &self,
        mut request: VideoGenerationRequest,
    ) -> Result<VideoJob, AgentError> {
        let snapshot = self.catalogs.runtime.snapshot();
        let inputs = [Content::from(request.prompt.clone())];
        self.check_inputs(&inputs)?;
        if request.seconds == 0 {
            return Err(invalid("video duration must be positive"));
        }
        let binding = snapshot.video.select(&request.choice, &inputs)?;
        request.choice.model = Some(binding.model.clone());
        let mut job = self
            .dispatch(
                ModelOperation::VideoGeneration,
                &binding.model,
                binding.implementation.submit(&binding.model.model, request),
            )
            .await?;
        validate_job(&job, &binding.model, &binding.protocol)?;
        job.binding_id = Some(RunId::new_v4());
        self.catalogs
            .runtime
            .retain_video_job(&job, binding.clone());
        Ok(job)
    }
    async fn query_video(&self, job: &VideoJob) -> Result<VideoJob, AgentError> {
        let binding = self.video_binding(job)?;
        let mut next = self
            .dispatch(
                ModelOperation::VideoGeneration,
                &binding.model,
                binding.implementation.query(job),
            )
            .await?;
        validate_job(&next, &binding.model, &binding.protocol)?;
        if next.id != job.id {
            return Err(invalid("video query changed the job ID"));
        }
        next.binding_id = job.binding_id;
        Ok(next)
    }
    async fn cancel_video(&self, job: &VideoJob) -> Result<(), AgentError> {
        let binding = self.video_binding(job)?;
        if !binding.capabilities.cancellation {
            return Err(invalid("video binding does not support cancellation"));
        }
        self.dispatch(
            ModelOperation::VideoGeneration,
            &binding.model,
            binding.implementation.cancel(job),
        )
        .await
    }
    async fn transcribe(
        &self,
        mut request: SpeechRecognitionRequest,
    ) -> Result<SpeechRecognitionResponse, AgentError> {
        let snapshot = self.catalogs.runtime.snapshot();
        let inputs = [Content(vec![ContentBlock::Media {
            media: request.audio.clone(),
        }])];
        self.check_inputs(&inputs)?;
        if !request.audio.mime_type.starts_with("audio/") {
            return Err(invalid("speech recognition requires audio"));
        }
        let binding = snapshot.recognition.select(&request.choice, &inputs)?;
        request.choice.model = Some(binding.model.clone());
        let response = self
            .dispatch(
                ModelOperation::SpeechRecognition,
                &binding.model,
                binding
                    .implementation
                    .transcribe(&binding.model.model, request),
            )
            .await?;
        if response.model != binding.model
            || response.segments.iter().any(|s| {
                !s.start_seconds.is_finite()
                    || !s.end_seconds.is_finite()
                    || s.start_seconds < 0.0
                    || s.end_seconds < s.start_seconds
            })
        {
            return Err(invalid("invalid speech recognition response"));
        }
        Ok(response)
    }
    async fn synthesize(
        &self,
        mut request: SpeechSynthesisRequest,
    ) -> Result<SpeechSynthesisResponse, AgentError> {
        let snapshot = self.catalogs.runtime.snapshot();
        let inputs = [Content::from(request.text.clone())];
        self.check_inputs(&inputs)?;
        if request.voice.trim().is_empty() || request.format.trim().is_empty() {
            return Err(invalid("speech voice and format are required"));
        }
        let binding = snapshot.synthesis.select(&request.choice, &inputs)?;
        request.choice.model = Some(binding.model.clone());
        let response = self
            .dispatch(
                ModelOperation::SpeechSynthesis,
                &binding.model,
                binding
                    .implementation
                    .synthesize(&binding.model.model, request),
            )
            .await?;
        if response.model != binding.model || !response.audio.mime_type.starts_with("audio/") {
            return Err(invalid("invalid speech synthesis response"));
        }
        Ok(response)
    }
    async fn detect_content(
        &self,
        mut request: ContentDetectionRequest,
    ) -> Result<ContentDetectionResponse, AgentError> {
        let snapshot = self.catalogs.runtime.snapshot();
        let inputs = [request.content.clone()];
        self.check_inputs(&inputs)?;
        let binding = snapshot.detection.select(&request.choice, &inputs)?;
        request.choice.model = Some(binding.model.clone());
        let response = self
            .dispatch(
                ModelOperation::ContentDetection,
                &binding.model,
                binding.implementation.detect(&binding.model.model, request),
            )
            .await?;
        if response.model != binding.model {
            return Err(invalid(
                "content detection response model does not match binding",
            ));
        }
        Ok(response)
    }
    async fn connect_realtime(
        &self,
        mut request: RealtimeRequest,
    ) -> Result<Arc<dyn RealtimeSession>, AgentError> {
        let snapshot = self.catalogs.runtime.snapshot();
        let binding = snapshot.realtime.select_modalities(
            &request.choice,
            &request.input,
            &request.output,
        )?;
        if request.input.is_empty()
            || request.output.is_empty()
            || request
                .input
                .iter()
                .any(|m| !binding.capabilities.input.contains(m))
            || request
                .output
                .iter()
                .any(|m| !binding.capabilities.output.contains(m))
        {
            return Err(invalid("unsupported realtime modalities"));
        }
        request.choice.model = Some(binding.model.clone());
        let cancellation = SessionCancellation(self.catalogs.stopped.child_token());
        let input = request.input.clone();
        let output = request.output.clone();
        let session = self
            .dispatch(
                ModelOperation::Realtime,
                &binding.model,
                binding.implementation.connect(
                    &binding.model.model,
                    request,
                    cancellation.0.clone(),
                ),
            )
            .await?;
        Ok(Arc::new(ManagedSession {
            session,
            cancellation,
            options: self.options.clone(),
            input,
            output,
        }))
    }
}
fn invalid(message: &str) -> AgentError {
    AgentError::InvalidConfiguration(message.into())
}

struct SessionCancellation(CancellationToken);
impl Drop for SessionCancellation {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
struct ManagedSession {
    session: Arc<dyn RealtimeSession>,
    cancellation: SessionCancellation,
    options: ModelInvocationOptions,
    input: Vec<Modality>,
    output: Vec<Modality>,
}
impl ManagedSession {
    async fn dispatch<T>(
        &self,
        future: impl Future<Output = Result<T, ModelError>>,
    ) -> Result<T, ModelError> {
        tokio::select! {
            biased;
            _ = self.cancellation.0.cancelled() => Err(ModelError::Fatal("realtime session closed".into())),
            _ = self.options.cancellation.cancelled() => { self.cancellation.0.cancel(); Err(ModelError::Fatal("realtime session cancelled".into())) },
            result = tokio::time::timeout(self.options.timeout, future) => result.map_err(|_| ModelError::Transient("realtime operation timed out".into()))?,
        }
    }
}
#[async_trait]
impl RealtimeSession for ManagedSession {
    async fn send(&self, input: RealtimeInput) -> Result<(), ModelError> {
        if let RealtimeInput::Content(content) = &input
            && (content.modalities().iter().any(|m| !self.input.contains(m))
                || serde_json::to_vec(content)
                    .map_or(true, |v| v.len() > self.options.max_input_bytes))
        {
            return Err(ModelError::Fatal(
                "unsupported or oversized realtime input".into(),
            ));
        }
        self.dispatch(self.session.send(input)).await
    }
    async fn next(&self) -> Result<Option<RealtimeEvent>, ModelError> {
        let event = self.dispatch(self.session.next()).await?;
        if let Some(RealtimeEvent::Content(content)) = &event
            && content
                .modalities()
                .iter()
                .any(|m| !self.output.contains(m))
        {
            return Err(ModelError::Fatal(
                "unexpected realtime output modality".into(),
            ));
        }
        Ok(event)
    }
    async fn close(&self) -> Result<(), ModelError> {
        self.cancellation.0.cancel();
        tokio::time::timeout(self.options.timeout, self.session.close())
            .await
            .map_err(|_| ModelError::Transient("realtime close timed out".into()))?
    }
}
fn validate_job(job: &VideoJob, model: &ModelRef, protocol: &str) -> Result<(), AgentError> {
    if &job.model != model
        || job.protocol != protocol
        || job.id.trim().is_empty()
        || job
            .progress
            .is_some_and(|p| !p.is_finite() || !(0.0..=1.0).contains(&p))
        || (job.status == ModelJobStatus::Completed && job.output.is_none())
        || (job.status == ModelJobStatus::Failed && job.error.is_none())
    {
        return Err(invalid("invalid video job response"));
    }
    Ok(())
}
