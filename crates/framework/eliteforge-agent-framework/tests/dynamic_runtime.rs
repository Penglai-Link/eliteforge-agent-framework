use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_test_support::TestBuilder;
use std::sync::Arc;

struct EchoModel;
#[async_trait]
impl ChatModel for EchoModel {
    async fn generate(
        &self,
        _model_id: &str,
        _request: ChatRequest,
        _stream: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        Ok(ChatResponse::text("ok"))
    }
}

fn connection(id: &str, model: &str) -> ModelConnection {
    ModelConnection::new(id, Arc::new(EchoModel), vec![ModelDescriptor::new(model)])
}

#[tokio::test]
async fn agent_build_preserves_host_initialized_and_replaced_runtime_bindings() {
    for replace_first in [false, true] {
        let mut update = ModelRuntimeUpdate::new("alice", 1, vec![connection("host", "main")]);
        update.video.push(VideoGenerationModelService::new(
            ModelRef::new("host", "video"),
            Arc::new(QueuedVideo(0.2)),
        ));
        let runtime = if replace_first {
            let runtime = ModelRuntime::new(ModelRuntimeUpdate::new("", 0, vec![])).unwrap();
            runtime.replace(update).unwrap();
            runtime
        } else {
            ModelRuntime::new(update).unwrap()
        };
        // A host-supplied runtime is itself a model registration; automatic
        // defaults must not require MODEL or install an extra provider.
        let standalone = eliteforge_agent_test_support::TestBuilder::default()
            .default_models(true)
            .plugin(ModelRuntimePlugin::new(runtime.clone()))
            .plugin(ModelStrategyPlugin(Arc::new(FixedModelStrategy(
                ModelRef::new("host", "main"),
            ))))
            .build()
            .await
            .unwrap();
        assert_eq!(
            standalone.models(ModelOperation::Chat),
            vec![ModelRef::new("host", "main")]
        );
        assert_eq!(
            standalone
                .run(RunRequest::new("standalone", "hello"))
                .await
                .unwrap()
                .output,
            "ok"
        );
        standalone.framework().close().await;
        let agent = eliteforge_agent_test_support::TestBuilder::default()
            .default_models(false)
            .plugin(ModelRuntimePlugin::new(runtime.clone()))
            .plugin(ModelConnectionPlugin::new(connection("local", "other")))
            .plugin(ModelStrategyPlugin(Arc::new(FixedModelStrategy(
                ModelRef::new("host", "main"),
            ))))
            .build()
            .await
            .unwrap();
        assert_eq!(
            agent.models(ModelOperation::Chat),
            vec![
                ModelRef::new("host", "main"),
                ModelRef::new("local", "other")
            ]
        );
        assert_eq!(
            agent.models(ModelOperation::VideoGeneration),
            vec![ModelRef::new("host", "video")]
        );
        assert_eq!(
            agent
                .run(RunRequest::new("initialized", "hello"))
                .await
                .unwrap()
                .output,
            "ok"
        );
        runtime
            .replace_connection(
                "dynamic",
                ModelRuntimeUpdate::new("alice", 1, vec![connection("dynamic", "new")]),
            )
            .unwrap();
        assert_eq!(agent.models(ModelOperation::Chat).len(), 3);
        assert!(
            runtime
                .snapshot()
                .chat
                .get(&ModelRef::new("host", "main"))
                .is_some()
        );
        assert!(
            runtime
                .snapshot()
                .chat
                .get(&ModelRef::new("local", "other"))
                .is_some()
        );
        agent.framework().close().await;
    }
}

#[test]
fn provider_refresh_keeps_other_connections_and_old_snapshots() {
    let runtime = ModelRuntime::new(ModelRuntimeUpdate::new(
        "",
        0,
        vec![connection("local", "a")],
    ))
    .unwrap();
    runtime
        .replace_connection(
            "personai",
            ModelRuntimeUpdate::new("alice", 1, vec![connection("personai", "b")]),
        )
        .unwrap();
    let in_flight = runtime.snapshot();
    assert!(in_flight.chat.get(&ModelRef::new("local", "a")).is_some());
    assert!(
        in_flight
            .chat
            .get(&ModelRef::new("personai", "b"))
            .is_some()
    );

    runtime
        .replace_connection("personai", ModelRuntimeUpdate::new("alice", 1, vec![]))
        .unwrap();
    let current = runtime.snapshot();
    assert!(current.chat.get(&ModelRef::new("local", "a")).is_some());
    assert!(current.chat.get(&ModelRef::new("personai", "b")).is_none());
    assert!(
        in_flight
            .chat
            .get(&ModelRef::new("personai", "b"))
            .is_some()
    );
}

#[test]
fn delayed_subscription_cannot_overwrite_a_newer_recovery_or_account_invalidation() {
    let runtime = ModelRuntime::new(ModelRuntimeUpdate::new("", 0, vec![])).unwrap();
    let mut old = ModelRuntimeUpdate::new("alice", 1, vec![connection("personai", "old")]);
    old.generation = 1;
    let mut fresh = ModelRuntimeUpdate::new("alice", 1, vec![connection("personai", "new")]);
    fresh.generation = 2;
    runtime.replace_connection("personai", fresh).unwrap();
    runtime.replace_connection("personai", old.clone()).unwrap();
    assert!(
        runtime
            .snapshot()
            .chat
            .get(&ModelRef::new("personai", "new"))
            .is_some()
    );
    assert!(
        runtime
            .snapshot()
            .chat
            .get(&ModelRef::new("personai", "old"))
            .is_none()
    );
    let mut closed = ModelRuntimeUpdate::new("", 0, vec![]);
    closed.generation = 3;
    runtime.replace_connection("personai", closed).unwrap();
    runtime.replace_connection("personai", old).unwrap();
    assert!(runtime.snapshot().chat.models().is_empty());
}

#[test]
fn late_account_refresh_cannot_replace_new_account() {
    let runtime = ModelRuntime::new(ModelRuntimeUpdate::new("", 0, vec![])).unwrap();
    runtime
        .replace_connection(
            "personai",
            ModelRuntimeUpdate::new("bob", 2, vec![connection("personai", "new")]),
        )
        .unwrap();
    let accepted = runtime
        .replace_connection_if_current(
            "personai",
            "alice",
            1,
            ModelRuntimeUpdate::new("alice", 1, vec![connection("personai", "old")]),
        )
        .unwrap();
    assert!(!accepted);
    assert!(
        runtime
            .snapshot()
            .chat
            .get(&ModelRef::new("personai", "new"))
            .is_some()
    );
}

struct FakeExternal;
#[async_trait]
impl ExternalAgent for FakeExternal {
    async fn run(
        &self,
        request: ExternalAgentRequest,
        sink: &dyn ExternalAgentEventSink,
    ) -> Result<ExternalAgentResult, AgentError> {
        assert_eq!(request.session.as_ref().unwrap().session_id, "remote-1");
        sink.emit(ExternalAgentEvent::Status("running".into()))
            .await;
        sink.emit(ExternalAgentEvent::TextDelta("done".into()))
            .await;
        Ok(ExternalAgentResult {
            content: Content::from("done"),
            structured: None,
            session: Some(ExternalSessionRef {
                session_id: "remote-1".into(),
                run_id: Some("run-2".into()),
            }),
            usage: Usage::default(),
        })
    }
}

#[tokio::test]
async fn external_loop_runs_without_framework_storage() {
    let agent = TestBuilder::new()
        .default_models(false)
        .plugin(AgentLoopPlugin::new(ExternalAgentLoop))
        .plugin(ExternalAgentPlugin::new(FakeExternal))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("local-1", "hello");
    request.extensions.insert(ExternalSessionRef {
        session_id: "remote-1".into(),
        run_id: None,
    });
    let mut handle = agent.start(request).unwrap();
    let mut events = agent.events(&mut handle).unwrap();
    let result = agent.wait(handle).await.unwrap();
    assert_eq!(result.output, "done");
    assert_eq!(
        result.external_session.unwrap().run_id.as_deref(),
        Some("run-2")
    );
    let mut status = false;
    let mut session = false;
    while let Ok(envelope) = events.try_recv() {
        match envelope.event {
            AgentEvent::ExternalAgent(ExternalAgentEvent::Status(value)) if value == "running" => {
                status = true
            }
            AgentEvent::ExternalAgent(ExternalAgentEvent::Session(value))
                if value.run_id.as_deref() == Some("run-2") =>
            {
                session = true
            }
            _ => {}
        }
    }
    assert!(status && session);
}

#[tokio::test]
async fn external_loop_rejects_framework_resume() {
    let agent = TestBuilder::new()
        .default_models(false)
        .plugin(AgentLoopPlugin::new(ExternalAgentLoop))
        .plugin(ExternalAgentPlugin::new(FakeExternal))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("local-1", "hello");
    request.resume = Some(ResumeRequest::default());
    let error = match agent.start(request) {
        Ok(_) => panic!("external loop accepted Framework resume"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("resume"));
}

struct PendingExternal;
#[async_trait]
impl ExternalAgent for PendingExternal {
    async fn run(
        &self,
        _request: ExternalAgentRequest,
        _sink: &dyn ExternalAgentEventSink,
    ) -> Result<ExternalAgentResult, AgentError> {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        unreachable!("cancelled external invocation continued")
    }
}

#[tokio::test]
async fn external_loop_observes_cancel_and_timeout() {
    let agent = TestBuilder::new()
        .default_models(false)
        .plugin(AgentLoopPlugin::new(ExternalAgentLoop))
        .plugin(ExternalAgentPlugin::new(PendingExternal))
        .build()
        .await
        .unwrap();
    let handle = agent.start(RunRequest::new("cancel", "hello")).unwrap();
    agent.cancel(&handle).unwrap();
    let cancelled = tokio::time::timeout(std::time::Duration::from_secs(2), agent.wait(handle))
        .await
        .unwrap();
    assert!(matches!(cancelled, Err(AgentError::Cancelled)));

    let mut request = RunRequest::new("timeout", "hello");
    request.limits.run_timeout = std::time::Duration::from_millis(20);
    let timed_out = tokio::time::timeout(std::time::Duration::from_secs(2), agent.run(request))
        .await
        .unwrap();
    assert!(timed_out.unwrap_err().to_string().contains("deadline"));
}

struct QueuedVideo(f64);
#[async_trait]
impl VideoGenerationModel for QueuedVideo {
    async fn submit(
        &self,
        _model: &str,
        request: VideoGenerationRequest,
    ) -> Result<VideoJob, ModelError> {
        Ok(VideoJob {
            model: request.choice.model.unwrap(),
            protocol: "mock".into(),
            id: "job-1".into(),
            binding_id: None,
            status: ModelJobStatus::Queued,
            progress: None,
            output: None,
            error: None,
        })
    }
    async fn query(&self, job: &VideoJob) -> Result<VideoJob, ModelError> {
        Ok(VideoJob {
            progress: Some(self.0),
            ..job.clone()
        })
    }
}

struct OperationOnlyLoop;

fn video_request(choice: ModelChoice) -> VideoGenerationRequest {
    VideoGenerationRequest {
        choice,
        prompt: "clip".into(),
        seconds: 2,
        size: None,
        first_frame: None,
        last_frame: None,
        reference_images: vec![],
        reference_videos: vec![],
        reference_audios: vec![],
        provider_options: Default::default(),
    }
}

#[tokio::test]
async fn retired_default_and_stage_routes_do_not_block_directory_updates() {
    let runtime = ModelRuntime::new(ModelRuntimeUpdate::new("", 0, vec![])).unwrap();
    let default = ModelRef::new("personai", "default-video");
    let staged = ModelRef::new("personai", "stage-video");
    let mut update = ModelRuntimeUpdate::new("alice", 1, vec![]);
    for model in [&default, &staged] {
        update.video.push(
            VideoGenerationModelService::new(model.clone(), Arc::new(QueuedVideo(0.2)))
                .protocol("mock"),
        );
    }
    runtime
        .replace_connection("personai", update.clone())
        .unwrap();
    let agent = TestBuilder::new()
        .default_models(false)
        .plugin(ModelRuntimePlugin::new(runtime.clone()))
        .plugin(ModelConnectionPlugin::new(connection("local", "other")))
        .plugin(AgentLoopPlugin::new(OperationOnlyLoop))
        .plugin(ModelRoutingPlugin(
            ModelRouting::default()
                .default_for(ModelOperation::VideoGeneration, default.clone())
                .stage_for(ModelOperation::VideoGeneration, "render", staged.clone()),
        ))
        .build()
        .await
        .unwrap();
    let choices = [
        ModelChoice::default(),
        ModelChoice {
            model: None,
            stage: "render".into(),
        },
    ];
    let job = agent
        .submit_video(video_request(choices[0].clone()))
        .await
        .unwrap();
    assert_eq!(job.model, default);
    assert_eq!(
        agent
            .submit_video(video_request(choices[1].clone()))
            .await
            .unwrap()
            .model,
        staged
    );
    // An actual empty response and a logout invalidation must both publish.
    for (scope, revision) in [("alice", 1), ("", 0)] {
        runtime
            .replace_connection("personai", ModelRuntimeUpdate::new(scope, revision, vec![]))
            .unwrap();
        assert!(agent.models(ModelOperation::VideoGeneration).is_empty());
        assert_eq!(
            agent.models(ModelOperation::Chat),
            vec![ModelRef::new("local", "other")]
        );
        for choice in &choices {
            assert!(matches!(
                agent.submit_video(video_request(choice.clone())).await,
                Err(AgentError::Model(ModelError::Unavailable(_)))
            ));
        }
        assert_eq!(agent.query_video(&job).await.unwrap().progress, Some(0.2));
        runtime
            .replace_connection("personai", update.clone())
            .unwrap();
        for (choice, expected) in choices.iter().zip([&default, &staged]) {
            assert_eq!(
                &agent
                    .submit_video(video_request(choice.clone()))
                    .await
                    .unwrap()
                    .model,
                expected
            );
        }
    }
    agent.framework().close().await;
}

struct UncertainMedia(Arc<std::sync::atomic::AtomicUsize>);
#[async_trait]
impl VideoGenerationModel for UncertainMedia {
    async fn submit(&self, _: &str, _: VideoGenerationRequest) -> Result<VideoJob, ModelError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(ModelError::Indeterminate(
            "connection lost after submission".into(),
        ))
    }
    async fn query(&self, _: &VideoJob) -> Result<VideoJob, ModelError> {
        unreachable!()
    }
    async fn cancel(&self, _: &VideoJob) -> Result<(), ModelError> {
        unreachable!()
    }
}
#[async_trait]
impl ImageGenerationModel for UncertainMedia {
    async fn generate(
        &self,
        _: &str,
        _: ImageGenerationRequest,
    ) -> Result<ImageGenerationResponse, ModelError> {
        unreachable!()
    }
    async fn submit(&self, _: &str, _: ImageGenerationRequest) -> Result<ImageJob, ModelError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(ModelError::Indeterminate(
            "connection lost after submission".into(),
        ))
    }
}

#[tokio::test]
async fn uncertain_media_submissions_are_not_retried_by_chat_recovery_policy() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider = Arc::new(UncertainMedia(calls.clone()));
    let model = ModelRef::new("media", "async");
    let mut update = ModelRuntimeUpdate::new("alice", 1, vec![]);
    update.video.push(VideoGenerationModelService::new(
        model.clone(),
        provider.clone(),
    ));
    update
        .image
        .push(ImageGenerationModelService::new(model.clone(), provider));
    let runtime = ModelRuntime::new(ModelRuntimeUpdate::new("", 0, vec![])).unwrap();
    runtime.replace_connection("media", update).unwrap();
    let agent = TestBuilder::new()
        .default_models(false)
        .plugin(ModelRuntimePlugin::new(runtime))
        .plugin(AgentLoopPlugin::new(OperationOnlyLoop))
        .plugin(ModelRecoveryPlugin(ModelRecoveryPolicy {
            candidates: vec![model.clone()],
            allow_explicit_model_replacement: true,
        }))
        .build()
        .await
        .unwrap();
    assert!(
        agent
            .submit_video(VideoGenerationRequest {
                choice: ModelChoice::model(model.clone()),
                prompt: "clip".into(),
                seconds: 2,
                size: None,
                first_frame: None,
                last_frame: None,
                reference_images: vec![],
                reference_videos: vec![],
                reference_audios: vec![],
                provider_options: Default::default(),
            })
            .await
            .is_err()
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(
        agent
            .submit_image(ImageGenerationRequest {
                choice: ModelChoice::model(model),
                prompt: "image".into(),
                size: None,
                count: 1,
                quality: None,
                provider_options: Default::default(),
            })
            .await
            .is_err()
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[async_trait]
impl AgentLoop for OperationOnlyLoop {
    fn requires_conversations(&self) -> bool {
        false
    }
    async fn run(
        &self,
        _request: &RunRequest,
        _context: &RunContext,
        _services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        Err(AgentError::Run("not used".into()))
    }
}

#[tokio::test]
async fn retired_video_job_keeps_its_original_binding() {
    let runtime = ModelRuntime::new(ModelRuntimeUpdate::new("", 0, vec![])).unwrap();
    let agent = TestBuilder::new()
        .default_models(false)
        .plugin(ModelRuntimePlugin::new(runtime.clone()))
        .plugin(AgentLoopPlugin::new(OperationOnlyLoop))
        .build()
        .await
        .unwrap();
    let model = ModelRef::new("personai", "video");
    let mut update = ModelRuntimeUpdate::new("alice", 1, vec![]);
    update.video.push(
        VideoGenerationModelService::new(model.clone(), Arc::new(QueuedVideo(0.1)))
            .protocol("mock"),
    );
    runtime.replace_connection("personai", update).unwrap();
    assert_eq!(
        agent.models(ModelOperation::VideoGeneration),
        vec![model.clone()]
    );
    let job = agent
        .submit_video(VideoGenerationRequest {
            choice: ModelChoice::model(model.clone()),
            prompt: "clip".into(),
            seconds: 2,
            size: None,
            first_frame: None,
            last_frame: None,
            reference_images: vec![],
            reference_videos: vec![],
            reference_audios: vec![],
            provider_options: Default::default(),
        })
        .await
        .unwrap();
    runtime
        .replace_connection("personai", ModelRuntimeUpdate::new("alice", 1, vec![]))
        .unwrap();
    assert!(agent.models(ModelOperation::VideoGeneration).is_empty());
    assert_eq!(agent.query_video(&job).await.unwrap().progress, Some(0.1));
}

#[tokio::test]
async fn video_job_retains_binding_across_same_model_token_refresh() {
    let runtime = ModelRuntime::new(ModelRuntimeUpdate::new("", 0, vec![])).unwrap();
    let agent = TestBuilder::new()
        .default_models(false)
        .plugin(ModelRuntimePlugin::new(runtime.clone()))
        .plugin(AgentLoopPlugin::new(OperationOnlyLoop))
        .build()
        .await
        .unwrap();
    let model = ModelRef::new("personai", "video");
    let mut first = ModelRuntimeUpdate::new("alice", 1, vec![]);
    first.video.push(
        VideoGenerationModelService::new(model.clone(), Arc::new(QueuedVideo(0.1)))
            .protocol("mock"),
    );
    runtime.replace_connection("personai", first).unwrap();
    let job = agent
        .submit_video(VideoGenerationRequest {
            choice: ModelChoice::model(model.clone()),
            prompt: "clip".into(),
            seconds: 2,
            size: None,
            first_frame: None,
            last_frame: None,
            reference_images: vec![],
            reference_videos: vec![],
            reference_audios: vec![],
            provider_options: Default::default(),
        })
        .await
        .unwrap();
    let mut refreshed = ModelRuntimeUpdate::new("alice", 2, vec![]);
    refreshed
        .video
        .push(VideoGenerationModelService::new(model, Arc::new(QueuedVideo(0.9))).protocol("mock"));
    runtime.replace_connection("personai", refreshed).unwrap();
    assert_eq!(agent.query_video(&job).await.unwrap().progress, Some(0.1));
}

#[tokio::test]
async fn identical_remote_video_ids_across_accounts_keep_distinct_bindings() {
    let runtime = ModelRuntime::new(ModelRuntimeUpdate::new("", 0, vec![])).unwrap();
    let agent = TestBuilder::new()
        .default_models(false)
        .plugin(ModelRuntimePlugin::new(runtime.clone()))
        .plugin(AgentLoopPlugin::new(OperationOnlyLoop))
        .build()
        .await
        .unwrap();
    let model = ModelRef::new("personai", "video");
    let request = || VideoGenerationRequest {
        choice: ModelChoice::model(model.clone()),
        prompt: "clip".into(),
        seconds: 2,
        size: None,
        first_frame: None,
        last_frame: None,
        reference_images: vec![],
        reference_videos: vec![],
        reference_audios: vec![],
        provider_options: Default::default(),
    };
    let mut alice = ModelRuntimeUpdate::new("alice", 1, vec![]);
    alice.video.push(
        VideoGenerationModelService::new(model.clone(), Arc::new(QueuedVideo(0.1)))
            .protocol("mock"),
    );
    runtime.replace_connection("personai", alice).unwrap();
    let alice_job = agent.submit_video(request()).await.unwrap();

    let mut bob = ModelRuntimeUpdate::new("bob", 2, vec![]);
    bob.video.push(
        VideoGenerationModelService::new(model.clone(), Arc::new(QueuedVideo(0.9)))
            .protocol("mock"),
    );
    runtime.replace_connection("personai", bob).unwrap();
    let bob_job = agent.submit_video(request()).await.unwrap();

    assert_eq!(alice_job.id, bob_job.id);
    assert_eq!(alice_job.model, bob_job.model);
    assert_ne!(alice_job.binding_id, bob_job.binding_id);
    assert_eq!(
        agent.query_video(&alice_job).await.unwrap().progress,
        Some(0.1)
    );
    assert_eq!(
        agent.query_video(&bob_job).await.unwrap().progress,
        Some(0.9)
    );

    let restored: VideoJob =
        serde_json::from_value(serde_json::to_value(&alice_job).unwrap()).unwrap();
    let fresh_runtime = ModelRuntime::new(ModelRuntimeUpdate::new("", 0, vec![])).unwrap();
    let mut refreshed = ModelRuntimeUpdate::new("alice", 1, vec![]);
    refreshed
        .video
        .push(VideoGenerationModelService::new(model, Arc::new(QueuedVideo(0.7))).protocol("mock"));
    fresh_runtime
        .replace_connection("personai", refreshed)
        .unwrap();
    let fresh_agent = TestBuilder::new()
        .default_models(false)
        .plugin(ModelRuntimePlugin::new(fresh_runtime))
        .plugin(AgentLoopPlugin::new(OperationOnlyLoop))
        .build()
        .await
        .unwrap();
    let error = fresh_agent.query_video(&restored).await.unwrap_err();
    assert!(matches!(
        error,
        AgentError::Model(ModelError::Unavailable(_))
    ));
}

struct QueuedImage(f64);
#[async_trait]
impl ImageGenerationModel for QueuedImage {
    async fn generate(
        &self,
        _model: &str,
        _request: ImageGenerationRequest,
    ) -> Result<ImageGenerationResponse, ModelError> {
        unreachable!("async image submission must not use synchronous generation")
    }
    async fn submit(
        &self,
        _model: &str,
        request: ImageGenerationRequest,
    ) -> Result<ImageJob, ModelError> {
        Ok(ImageJob {
            model: request.choice.model.unwrap(),
            protocol: "mock".into(),
            id: "job-1".into(),
            binding_id: None,
            status: ModelJobStatus::Queued,
            progress: None,
            images: vec![],
            error: None,
        })
    }
    async fn query(&self, job: &ImageJob) -> Result<ImageJob, ModelError> {
        Ok(ImageJob {
            progress: Some(self.0),
            ..job.clone()
        })
    }
}

#[tokio::test]
async fn identical_remote_image_ids_across_accounts_keep_distinct_bindings() {
    let runtime = ModelRuntime::new(ModelRuntimeUpdate::new("", 0, vec![])).unwrap();
    let agent = TestBuilder::new()
        .default_models(false)
        .plugin(ModelRuntimePlugin::new(runtime.clone()))
        .plugin(AgentLoopPlugin::new(OperationOnlyLoop))
        .build()
        .await
        .unwrap();
    let model = ModelRef::new("personai", "image");
    let request = || ImageGenerationRequest {
        choice: ModelChoice::model(model.clone()),
        prompt: "image".into(),
        size: None,
        count: 1,
        quality: None,
        provider_options: Default::default(),
    };
    let mut alice = ModelRuntimeUpdate::new("alice", 1, vec![]);
    alice.image.push(
        ImageGenerationModelService::new(model.clone(), Arc::new(QueuedImage(0.1)))
            .protocol("mock"),
    );
    runtime.replace_connection("personai", alice).unwrap();
    let alice_job = agent.submit_image(request()).await.unwrap();

    let mut bob = ModelRuntimeUpdate::new("bob", 2, vec![]);
    bob.image.push(
        ImageGenerationModelService::new(model.clone(), Arc::new(QueuedImage(0.9)))
            .protocol("mock"),
    );
    runtime.replace_connection("personai", bob).unwrap();
    let bob_job = agent.submit_image(request()).await.unwrap();
    assert_eq!(alice_job.id, bob_job.id);
    assert_ne!(alice_job.binding_id, bob_job.binding_id);
    assert_eq!(
        agent.query_image(&alice_job).await.unwrap().progress,
        Some(0.1)
    );
    assert_eq!(
        agent.query_image(&bob_job).await.unwrap().progress,
        Some(0.9)
    );

    let restored: ImageJob =
        serde_json::from_value(serde_json::to_value(&alice_job).unwrap()).unwrap();
    let fresh_agent = TestBuilder::new()
        .default_models(false)
        .plugin(ModelRuntimePlugin::new(
            ModelRuntime::new(ModelRuntimeUpdate::new("", 0, vec![])).unwrap(),
        ))
        .plugin(AgentLoopPlugin::new(OperationOnlyLoop))
        .build()
        .await
        .unwrap();
    let error = fresh_agent.query_image(&restored).await.unwrap_err();
    assert!(matches!(
        error,
        AgentError::Model(ModelError::Unavailable(_))
    ));
}

#[test]
fn provider_refresh_cannot_change_another_connections_embedding_space() {
    let runtime = ModelRuntime::new(ModelRuntimeUpdate::new(
        "",
        0,
        vec![connection("local", "a")],
    ))
    .unwrap();
    let previous = runtime.snapshot();
    let mut update = ModelRuntimeUpdate::new("alice", 1, vec![connection("personai", "b")]);
    update.spaces.push(EmbeddingSpace {
        model: ModelRef::new("local", "embedding"),
        revision: "weights-v1".into(),
        dimensions: 3,
        preprocessing: "plain".into(),
    });
    let error = runtime
        .replace_connection("personai", update.clone())
        .unwrap_err();
    assert!(error.to_string().contains("another connection"));
    assert!(Arc::ptr_eq(&previous, &runtime.snapshot()));
    assert!(
        runtime
            .snapshot()
            .chat
            .get(&ModelRef::new("local", "a"))
            .is_some()
    );
    assert!(
        runtime
            .snapshot()
            .chat
            .get(&ModelRef::new("personai", "b"))
            .is_none()
    );

    // The provider may retain its own space after a model is retired; only
    // ownership, not model availability, is a condition for dynamic updates.
    update.spaces[0].model.connection = "personai".into();
    runtime.replace_connection("personai", update).unwrap();
    assert!(
        runtime
            .snapshot()
            .chat
            .get(&ModelRef::new("local", "a"))
            .is_some()
    );
    assert!(
        runtime
            .snapshot()
            .chat
            .get(&ModelRef::new("personai", "b"))
            .is_some()
    );
}
