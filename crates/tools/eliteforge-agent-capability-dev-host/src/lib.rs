//! Offline model adapters used by the SDK example and its process tests.
use async_trait::async_trait;
use eliteforge_agent_framework::*;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
pub struct DemoMedia;
pub fn media(mime: &str) -> MediaContent {
    MediaContent {
        mime_type: mime.into(),
        source: MediaSource::Base64("AA==".into()),
        name: None,
        estimated_tokens: Some(1),
    }
}
#[async_trait]
impl EmbeddingModel for DemoMedia {
    async fn embed(
        &self,
        model: &str,
        request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, ModelError> {
        Ok(EmbeddingResponse {
            provider_model: None,
            model: ModelRef::new("demo", model),
            input_tokens: Some(2),
            vectors: request
                .inputs
                .iter()
                .enumerate()
                .map(|(index, _)| EmbeddingVector {
                    index,
                    values: vec![1.; request.dimensions.unwrap_or(3)],
                })
                .collect(),
        })
    }
}
#[async_trait]
impl RerankModel for DemoMedia {
    async fn rerank(
        &self,
        model: &str,
        request: RerankRequest,
    ) -> Result<RerankResponse, ModelError> {
        Ok(RerankResponse {
            model: ModelRef::new("demo", model),
            results: request
                .documents
                .iter()
                .enumerate()
                .take(request.top_n.unwrap_or(request.documents.len()))
                .map(|(index, _)| RankedDocument { index, score: 1. })
                .collect(),
        })
    }
}
#[async_trait]
impl ImageGenerationModel for DemoMedia {
    async fn generate(
        &self,
        model: &str,
        request: ImageGenerationRequest,
    ) -> Result<ImageGenerationResponse, ModelError> {
        Ok(ImageGenerationResponse {
            model: ModelRef::new("demo", model),
            images: vec![media("image/png"); request.count as usize],
        })
    }
}
#[async_trait]
impl VideoGenerationModel for DemoMedia {
    async fn submit(&self, model: &str, _: VideoGenerationRequest) -> Result<VideoJob, ModelError> {
        Ok(VideoJob {
            model: ModelRef::new("demo", model),
            protocol: "demo/video".into(),
            id: "one".into(),
            binding_id: None,
            status: ModelJobStatus::Queued,
            progress: None,
            output: None,
            error: None,
        })
    }
    async fn query(&self, job: &VideoJob) -> Result<VideoJob, ModelError> {
        Ok(VideoJob {
            status: ModelJobStatus::Completed,
            progress: Some(1.),
            output: Some(ResourceReference {
                uri: "demo://video".into(),
                mime_type: Some("video/mp4".into()),
                name: None,
                size_bytes: None,
            }),
            ..job.clone()
        })
    }
    async fn cancel(&self, _: &VideoJob) -> Result<(), ModelError> {
        Ok(())
    }
}
#[async_trait]
impl SpeechRecognitionModel for DemoMedia {
    async fn transcribe(
        &self,
        model: &str,
        _: SpeechRecognitionRequest,
    ) -> Result<SpeechRecognitionResponse, ModelError> {
        Ok(SpeechRecognitionResponse {
            model: ModelRef::new("demo", model),
            text: "heard".into(),
            segments: vec![],
        })
    }
}
#[async_trait]
impl SpeechSynthesisModel for DemoMedia {
    async fn synthesize(
        &self,
        model: &str,
        _: SpeechSynthesisRequest,
    ) -> Result<SpeechSynthesisResponse, ModelError> {
        Ok(SpeechSynthesisResponse {
            model: ModelRef::new("demo", model),
            audio: media("audio/wav"),
        })
    }
}
struct DemoSession(CancellationToken);
#[async_trait]
impl RealtimeSession for DemoSession {
    async fn send(&self, _: RealtimeInput) -> Result<(), ModelError> {
        if self.0.is_cancelled() {
            return Err(ModelError::Fatal("session closed".into()));
        }
        Ok(())
    }
    async fn next(&self) -> Result<Option<RealtimeEvent>, ModelError> {
        Ok(Some(RealtimeEvent::Transcript("realtime".into())))
    }
    async fn close(&self) -> Result<(), ModelError> {
        self.0.cancel();
        Ok(())
    }
}
impl Drop for DemoSession {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
#[async_trait]
impl RealtimeModel for DemoMedia {
    async fn connect(
        &self,
        _: &str,
        _: RealtimeRequest,
        cancellation: CancellationToken,
    ) -> Result<Arc<dyn RealtimeSession>, ModelError> {
        Ok(Arc::new(DemoSession(cancellation)))
    }
}
