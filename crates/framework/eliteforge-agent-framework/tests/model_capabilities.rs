use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_test_support::*;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

struct Echo;
#[async_trait]
impl ChatModel for Echo {
    async fn generate(
        &self,
        model: &str,
        _: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        Ok(ChatResponse::text(model))
    }
}
struct Register<T>(String, T);
#[async_trait]
impl<T: Clone + Send + Sync + 'static> Plugin for Register<T> {
    fn id(&self) -> &str {
        &self.0
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        context.contribute(self.1.clone())
    }
}
#[derive(Default)]
struct Embedder {
    calls: Mutex<Vec<EmbeddingRequest>>,
}
#[async_trait]
impl EmbeddingModel for Embedder {
    async fn embed(
        &self,
        model: &str,
        request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, ModelError> {
        self.calls.lock().unwrap().push(request.clone());
        Ok(EmbeddingResponse {
            provider_model: None,
            model: ModelRef::new("local", model),
            input_tokens: None,
            vectors: (0..request.inputs.len())
                .rev()
                .map(|index| EmbeddingVector {
                    index,
                    values: vec![index as f32; request.dimensions.unwrap_or(3)],
                })
                .collect(),
        })
    }
}
fn registration(model: &str, adapter: Arc<Embedder>) -> Register<EmbeddingModelService> {
    Register(
        format!("embed.{model}"),
        EmbeddingModelService::new(ModelRef::new("local", model), adapter),
    )
}

#[tokio::test]
async fn multiple_embeddings_select_by_request_then_stage_then_default_and_pin_space() {
    let adapter = Arc::new(Embedder::default());
    let space = EmbeddingSpace {
        model: ModelRef::new("local", "a"),
        revision: "2026-09-09".into(),
        dimensions: 3,
        preprocessing: "plain/v1".into(),
    };
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(Echo))
        .plugin(registration("a", adapter.clone()))
        .plugin(registration("b", adapter.clone()))
        .plugin(Register(
            "index.space".into(),
            EmbeddingSpaceService(space.clone()),
        ))
        .plugin(ModelRoutingPlugin(
            ModelRouting::default()
                .default_for(ModelOperation::Embedding, ModelRef::new("local", "a"))
                .stage_for(
                    ModelOperation::Embedding,
                    "search",
                    ModelRef::new("local", "b"),
                ),
        ))
        .build()
        .await
        .unwrap();
    let models = agent.clone();
    assert_eq!(models.models(ModelOperation::Embedding).len(), 2);
    assert_eq!(
        models.models(ModelOperation::Chat),
        vec![ModelRef::new("default", "default")]
    );
    let mut request = EmbeddingRequest::texts(["hello", "world"]);
    let result = models.embed(request.clone()).await.unwrap();
    assert_eq!(result.model, space.model);
    assert_eq!(result.vectors[0].index, 0);
    assert_eq!(result.input_tokens, None);
    request.choice.stage = "search".into();
    assert_eq!(
        models.embed(request.clone()).await.unwrap().model.model,
        "b"
    );
    request.choice.model = Some(space.model.clone());
    assert_eq!(
        models.embed(request.clone()).await.unwrap().model.model,
        "a"
    );
    request.choice.model = Some(ModelRef::new("local", "missing"));
    assert!(models.embed(request).await.is_err());
    let query = models
        .embed_query(&space, EmbeddingRequest::texts(["q"]))
        .await
        .unwrap();
    assert_eq!(query.vectors[0].values.len(), 3);
    assert_eq!(
        adapter.calls.lock().unwrap().last().unwrap().purpose,
        EmbeddingPurpose::Query
    );
    for wrong in [
        EmbeddingSpace {
            revision: "other".into(),
            ..space.clone()
        },
        EmbeddingSpace {
            preprocessing: "other".into(),
            ..space.clone()
        },
        EmbeddingSpace {
            dimensions: 2,
            ..space.clone()
        },
    ] {
        assert!(
            models
                .embed_query(&wrong, EmbeddingRequest::texts(["q"]))
                .await
                .is_err()
        );
    }
    assert!(agent.framework().close().await.is_success());
    assert!(matches!(
        models.embed(EmbeddingRequest::texts(["closed"])).await,
        Err(AgentError::Closed)
    ));
}

#[tokio::test]
async fn duplicate_bindings_and_invalid_defaults_fail_at_build_ambiguity_at_call() {
    let adapter = Arc::new(Embedder::default());
    let service = EmbeddingModelService::new(ModelRef::new("local", "a"), adapter.clone());
    let duplicate = TestBuilder::default()
        .plugin(ChatModelPlugin::new(Echo))
        .plugin(Register("one".into(), service.clone()))
        .plugin(Register("two".into(), service))
        .build()
        .await;
    assert!(matches!(
        duplicate,
        Err(AgentError::InvalidConfiguration(_))
    ));
    let invalid = TestBuilder::default()
        .plugin(ChatModelPlugin::new(Echo))
        .plugin(registration("a", adapter.clone()))
        .plugin(ModelRoutingPlugin(ModelRouting::default().default_for(
            ModelOperation::Embedding,
            ModelRef::new("local", "hidden"),
        )))
        .build()
        .await;
    assert!(matches!(invalid, Err(AgentError::InvalidConfiguration(_))));
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(Echo))
        .plugin(registration("a", adapter.clone()))
        .plugin(registration("b", adapter.clone()))
        .build()
        .await
        .unwrap();
    assert!(
        agent
            .embed(EmbeddingRequest::texts(["hi"]))
            .await
            .unwrap_err()
            .to_string()
            .contains("multiple compatible")
    );
    assert!(adapter.calls.lock().unwrap().is_empty());
    agent.framework().close().await;
}

#[tokio::test]
async fn retired_embedding_with_host_space_is_unavailable_and_can_reappear() {
    let adapter = Arc::new(Embedder::default());
    let model = ModelRef::new("local", "a");
    let space = EmbeddingSpace {
        model: model.clone(),
        revision: "v1".into(),
        dimensions: 3,
        preprocessing: "plain/v1".into(),
    };
    let runtime = ModelRuntime::new(ModelRuntimeUpdate::new("", 0, vec![])).unwrap();
    let mut update = ModelRuntimeUpdate::new("alice", 1, vec![]);
    update
        .embedding
        .push(EmbeddingModelService::new(model.clone(), adapter.clone()));
    runtime.replace_connection("local", update.clone()).unwrap();
    let agent = TestBuilder::default()
        .plugin(ModelRuntimePlugin::new(runtime.clone()))
        .plugin(ChatModelPlugin::new(Echo))
        .plugin(Register(
            "index.space".into(),
            EmbeddingSpaceService(space.clone()),
        ))
        .plugin(ModelRoutingPlugin(
            ModelRouting::default().default_for(ModelOperation::Embedding, model.clone()),
        ))
        .build()
        .await
        .unwrap();
    runtime
        .replace_connection("local", ModelRuntimeUpdate::new("", 0, vec![]))
        .unwrap();
    assert!(agent.models(ModelOperation::Embedding).is_empty());
    assert!(matches!(
        agent.embed(EmbeddingRequest::texts(["q"])).await,
        Err(AgentError::Model(ModelError::Unavailable(_)))
    ));
    assert!(adapter.calls.lock().unwrap().is_empty());
    runtime.replace_connection("local", update).unwrap();
    let response = agent.embed(EmbeddingRequest::texts(["q"])).await.unwrap();
    assert_eq!(response.model, model);
    assert_eq!(response.vectors[0].values.len(), space.dimensions);
    assert_eq!(
        adapter.calls.lock().unwrap()[0].dimensions,
        Some(space.dimensions)
    );
    agent.framework().close().await;
}

#[tokio::test]
async fn multimodal_chat_filters_candidates_and_explicit_incompatible_choice_never_falls_back() {
    let agent = TestBuilder::default()
        .plugin(ModelConnectionPlugin::new(ModelConnection::new(
            "local",
            Arc::new(Echo),
            vec![
                ModelDescriptor::new("text"),
                ModelDescriptor::new("vision").with_input_modalities([
                    Modality::Text,
                    Modality::Image,
                    Modality::Video,
                ]),
            ],
        )))
        .build()
        .await
        .unwrap();
    for mime in ["image/png", "video/mp4"] {
        let mut request = RunRequest::new(
            mime,
            Content(vec![ContentBlock::Media { media: media(mime) }]),
        );
        assert_eq!(agent.run(request.clone()).await.unwrap().output, "vision");
        request.session_id = format!("explicit.{mime}");
        request.model = Some(ModelRef::new("local", "text"));
        assert!(agent.run(request).await.is_err());
    }
    agent.framework().close().await;
}
fn media(mime: &str) -> MediaContent {
    MediaContent {
        mime_type: mime.into(),
        source: MediaSource::Base64("YQ==".into()),
        name: None,
        estimated_tokens: Some(10),
    }
}

struct PendingEmbed {
    entered: Arc<Notify>,
    dropped: Arc<AtomicUsize>,
}
struct CountDrop(Arc<AtomicUsize>);
impl Drop for CountDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
#[async_trait]
impl EmbeddingModel for PendingEmbed {
    async fn embed(&self, _: &str, _: EmbeddingRequest) -> Result<EmbeddingResponse, ModelError> {
        let _guard = CountDrop(self.dropped.clone());
        self.entered.notify_one();
        std::future::pending().await
    }
}
#[tokio::test]
async fn operation_timeout_cancellation_and_agent_close_drop_inflight_calls() {
    let entered = Arc::new(Notify::new());
    let dropped = Arc::new(AtomicUsize::new(0));
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(Echo))
        .plugin(Register(
            "pending".into(),
            EmbeddingModelService::new(
                ModelRef::new("local", "pending"),
                Arc::new(PendingEmbed {
                    entered: entered.clone(),
                    dropped: dropped.clone(),
                }),
            ),
        ))
        .build()
        .await
        .unwrap();
    let options = ModelInvocationOptions {
        timeout: Duration::from_millis(10),
        ..Default::default()
    };
    assert!(
        agent
            .with_model_options(options)
            .embed(EmbeddingRequest::texts(["timeout"]))
            .await
            .is_err()
    );
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    entered.notified().await;
    let cancellation = CancellationToken::new();
    let client = agent.with_model_options(ModelInvocationOptions {
        cancellation: cancellation.clone(),
        ..Default::default()
    });
    let task = tokio::spawn(async move { client.embed(EmbeddingRequest::texts(["cancel"])).await });
    entered.notified().await;
    cancellation.cancel();
    assert!(matches!(task.await.unwrap(), Err(AgentError::Cancelled)));
    let client = agent.clone();
    let task = tokio::spawn(async move { client.embed(EmbeddingRequest::texts(["close"])).await });
    entered.notified().await;
    assert!(agent.framework().close().await.is_success());
    assert!(matches!(task.await.unwrap(), Err(AgentError::Closed)));
    assert_eq!(dropped.load(Ordering::SeqCst), 3);
}

struct MediaAdapter;
#[async_trait]
impl RerankModel for MediaAdapter {
    async fn rerank(
        &self,
        model: &str,
        request: RerankRequest,
    ) -> Result<RerankResponse, ModelError> {
        Ok(RerankResponse {
            model: ModelRef::new("media", model),
            results: (0..request.top_n.unwrap_or(request.documents.len()))
                .map(|index| RankedDocument {
                    index,
                    score: 1.0 / (1 + index) as f64,
                })
                .collect(),
        })
    }
}
#[async_trait]
impl ImageGenerationModel for MediaAdapter {
    async fn generate(
        &self,
        model: &str,
        request: ImageGenerationRequest,
    ) -> Result<ImageGenerationResponse, ModelError> {
        Ok(ImageGenerationResponse {
            model: ModelRef::new("media", model),
            images: vec![media("image/png"); request.count as usize],
        })
    }
}
#[async_trait]
impl VideoGenerationModel for MediaAdapter {
    async fn submit(&self, model: &str, _: VideoGenerationRequest) -> Result<VideoJob, ModelError> {
        Ok(VideoJob {
            model: ModelRef::new("media", model),
            protocol: "fixture/video".into(),
            id: "job-1".into(),
            binding_id: None,
            status: ModelJobStatus::Queued,
            progress: Some(0.0),
            output: None,
            error: None,
        })
    }
    async fn query(&self, job: &VideoJob) -> Result<VideoJob, ModelError> {
        Ok(VideoJob {
            status: ModelJobStatus::Completed,
            progress: Some(1.0),
            output: Some(ResourceReference {
                uri: "fixture://video/job-1".into(),
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
impl SpeechRecognitionModel for MediaAdapter {
    async fn transcribe(
        &self,
        model: &str,
        _: SpeechRecognitionRequest,
    ) -> Result<SpeechRecognitionResponse, ModelError> {
        Ok(SpeechRecognitionResponse {
            model: ModelRef::new("media", model),
            text: "hello".into(),
            segments: vec![],
        })
    }
}
#[async_trait]
impl SpeechSynthesisModel for MediaAdapter {
    async fn synthesize(
        &self,
        model: &str,
        _: SpeechSynthesisRequest,
    ) -> Result<SpeechSynthesisResponse, ModelError> {
        Ok(SpeechSynthesisResponse {
            model: ModelRef::new("media", model),
            audio: media("audio/wav"),
        })
    }
}
struct Session(CancellationToken);
#[async_trait]
impl RealtimeSession for Session {
    async fn send(&self, _: RealtimeInput) -> Result<(), ModelError> {
        Ok(())
    }
    async fn next(&self) -> Result<Option<RealtimeEvent>, ModelError> {
        Ok(Some(RealtimeEvent::Transcript("hello".into())))
    }
    async fn close(&self) -> Result<(), ModelError> {
        self.0.cancel();
        Ok(())
    }
}
#[async_trait]
impl RealtimeModel for MediaAdapter {
    async fn connect(
        &self,
        _: &str,
        _: RealtimeRequest,
        token: CancellationToken,
    ) -> Result<Arc<dyn RealtimeSession>, ModelError> {
        Ok(Arc::new(Session(token)))
    }
}

#[tokio::test]
async fn dedicated_operations_dispatch_and_video_job_survives_serialization() {
    let adapter = Arc::new(MediaAdapter);
    let reference = ModelRef::new("media", "multi");
    let audio = OperationCapabilities {
        input: vec![Modality::Audio],
        output: vec![Modality::Text],
        ..Default::default()
    };
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(Echo))
        .plugin(Register(
            "rerank".into(),
            RerankModelService::new(reference.clone(), adapter.clone()),
        ))
        .plugin(Register(
            "image".into(),
            ImageGenerationModelService::new(reference.clone(), adapter.clone()).capabilities(
                OperationCapabilities {
                    output: vec![Modality::Image],
                    ..Default::default()
                },
            ),
        ))
        .plugin(Register(
            "video".into(),
            VideoGenerationModelService::new(reference.clone(), adapter.clone())
                .protocol("fixture/video")
                .capabilities(OperationCapabilities {
                    output: vec![Modality::Video],
                    cancellation: true,
                    ..Default::default()
                }),
        ))
        .plugin(Register(
            "recognition".into(),
            SpeechRecognitionModelService::new(reference.clone(), adapter.clone())
                .capabilities(audio.clone()),
        ))
        .plugin(Register(
            "synthesis".into(),
            SpeechSynthesisModelService::new(reference.clone(), adapter.clone()).capabilities(
                OperationCapabilities {
                    output: vec![Modality::Audio],
                    ..Default::default()
                },
            ),
        ))
        .plugin(Register(
            "realtime".into(),
            RealtimeModelService::new(reference, adapter).capabilities(audio),
        ))
        .build()
        .await
        .unwrap();
    let models = agent.clone();
    assert_eq!(
        models
            .rerank(RerankRequest {
                choice: Default::default(),
                query: "query".into(),
                documents: vec!["one".into(), "two".into()],
                top_n: Some(1)
            })
            .await
            .unwrap()
            .results
            .len(),
        1
    );
    assert_eq!(
        models
            .generate_image(ImageGenerationRequest {
                choice: Default::default(),
                prompt: "image".into(),
                size: None,
                count: 2,
                quality: None,
                provider_options: Default::default(),
            })
            .await
            .unwrap()
            .images
            .len(),
        2
    );
    let job = models
        .submit_video(VideoGenerationRequest {
            choice: Default::default(),
            prompt: "video".into(),
            size: None,
            seconds: 4,
            first_frame: None,
            last_frame: None,
            reference_images: vec![],
            reference_videos: vec![],
            reference_audios: vec![],
            provider_options: Default::default(),
        })
        .await
        .unwrap();
    let restored: VideoJob = serde_json::from_str(&serde_json::to_string(&job).unwrap()).unwrap();
    assert_eq!(
        models.query_video(&restored).await.unwrap().status,
        ModelJobStatus::Completed
    );
    let wrong = VideoJob {
        protocol: "other".into(),
        ..restored.clone()
    };
    assert!(models.query_video(&wrong).await.is_err());
    models.cancel_video(&restored).await.unwrap();
    assert_eq!(
        models
            .transcribe(SpeechRecognitionRequest {
                choice: Default::default(),
                audio: media("audio/wav"),
                language: None
            })
            .await
            .unwrap()
            .text,
        "hello"
    );
    assert_eq!(
        models
            .synthesize(SpeechSynthesisRequest {
                choice: Default::default(),
                text: "hello".into(),
                voice: "fixture".into(),
                format: "wav".into()
            })
            .await
            .unwrap()
            .audio
            .mime_type,
        "audio/wav"
    );
    let session = models
        .connect_realtime(RealtimeRequest {
            choice: Default::default(),
            input: vec![Modality::Audio],
            output: vec![Modality::Text],
        })
        .await
        .unwrap();
    agent
        .send_realtime(&session, RealtimeInput::Commit)
        .await
        .unwrap();
    assert!(matches!(
        agent.next_realtime(&session).await.unwrap(),
        Some(RealtimeEvent::Transcript(_))
    ));
    assert!(
        agent
            .send_realtime(&session, RealtimeInput::Content("unsupported text".into()))
            .await
            .is_err()
    );
    agent.framework().close().await;
    assert!(agent.next_realtime(&session).await.is_err());
    agent.close_realtime(&session).await.unwrap();
}

#[test]
fn invalid_embedding_batches_are_rejected() {
    let vector = EmbeddingVector {
        index: 0,
        values: vec![1.0, 2.0],
    };
    let response = EmbeddingResponse {
        provider_model: None,
        model: ModelRef::new("local", "a"),
        vectors: vec![vector.clone()],
        input_tokens: None,
    };
    assert!(response.validate(1, Some(2)).is_ok());
    assert!(response.validate(1, Some(3)).is_err());
    assert!(response.validate(2, None).is_err());
    for vectors in [
        vec![vector.clone(), vector],
        vec![EmbeddingVector {
            index: 1,
            values: vec![1.0],
        }],
        vec![EmbeddingVector {
            index: 0,
            values: vec![f32::NAN],
        }],
        vec![EmbeddingVector {
            index: 0,
            values: vec![],
        }],
    ] {
        let response = EmbeddingResponse {
            provider_model: None,
            vectors,
            ..response.clone()
        };
        assert!(response.validate(response.vectors.len(), None).is_err());
    }
}

struct EmbeddingProbe;
#[async_trait]
impl AgentLoop for EmbeddingProbe {
    async fn run(
        &self,
        _: &RunRequest,
        context: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let limits = RunLimits {
            max_total_tokens: 10,
            ..Default::default()
        };
        let operations = services
            .operations
            .with_options(ModelInvocationOptions::for_run(context, &limits));
        let response = operations.embed(EmbeddingRequest::texts(["cost"])).await?;
        assert_eq!(response.input_tokens, None);
        assert_eq!(
            context.usage().input_tokens,
            4,
            "missing usage is conservatively charged"
        );
        assert!(
            operations
                .embed(EmbeddingRequest::texts(["too expensive"]))
                .await
                .is_err()
        );
        assert_eq!(
            context.usage().input_tokens,
            4,
            "rejected admission does not dispatch or charge"
        );
        Ok(RunResult {
            output: "done".into(),
            content: "done".into(),
            structured: None,
            interactions: vec![],
            status: RunStatus::Completed,
            usage: context.usage(),
            steps: 0,
            tool_calls: 0,
            external_session: None,
        })
    }
}
#[tokio::test]
async fn embedding_only_workflow_uses_operation_default_and_shares_run_budget() {
    let adapter = Arc::new(Embedder::default());
    let agent = TestBuilder::default()
        .plugin(AgentLoopPlugin::new(EmbeddingProbe))
        .plugin(registration("a", adapter.clone()))
        .plugin(ModelRoutingPlugin(ModelRouting::default().default_for(
            ModelOperation::Embedding,
            ModelRef::new("local", "a"),
        )))
        .build()
        .await
        .unwrap();
    let result = agent
        .run(RunRequest::new("embedding-only", "run"))
        .await
        .unwrap();
    assert_eq!(result.usage.input_tokens, 4);
    assert_eq!(adapter.calls.lock().unwrap().len(), 1);
    agent.framework().close().await;
}
