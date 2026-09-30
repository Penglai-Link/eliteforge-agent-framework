use async_trait::async_trait;
use eliteforge_agent_framework::*;
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
    let agent = AgentBuilder::new()
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
    let result = agent.run(request).await.unwrap();
    assert_eq!(result.output, "done");
    assert_eq!(
        result.external_session.unwrap().run_id.as_deref(),
        Some("run-2")
    );
}

struct QueuedVideo;
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
            status: ModelJobStatus::Queued,
            progress: None,
            output: None,
            error: None,
        })
    }
    async fn query(&self, job: &VideoJob) -> Result<VideoJob, ModelError> {
        Ok(job.clone())
    }
}

struct OperationOnlyLoop;
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
    let agent = AgentBuilder::new()
        .default_models(false)
        .plugin(ModelRuntimePlugin::new(runtime.clone()))
        .plugin(AgentLoopPlugin::new(OperationOnlyLoop))
        .build()
        .await
        .unwrap();
    let model = ModelRef::new("personai", "video");
    let mut update = ModelRuntimeUpdate::new("alice", 1, vec![]);
    update.video.push(
        VideoGenerationModelService::new(model.clone(), Arc::new(QueuedVideo)).protocol("mock"),
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
    assert_eq!(agent.query_video(&job).await.unwrap().id, "job-1");
}
