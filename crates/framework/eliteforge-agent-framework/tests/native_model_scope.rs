use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_test_support::TestBuilder;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Default)]
struct Capture {
    requests: Mutex<Vec<(String, ChatRequest)>>,
}
#[async_trait]
impl ChatModel for Capture {
    async fn generate(
        &self,
        model: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        self.requests.lock().unwrap().push((model.into(), request));
        if model == "removed" {
            return Err(ModelError::Unavailable("catalog removed model".into()));
        }
        Ok(ChatResponse {
            usage: Usage {
                input_tokens: 4,
                output_tokens: 2,
            },
            ..ChatResponse::text(model)
        })
    }
}
#[derive(Default)]
struct Reports(Mutex<Vec<ModelDispatchReport>>);
#[async_trait]
impl EventSink for Reports {
    fn on_published(&self, _: &RunContext, envelope: &EventEnvelope) {
        if let AgentEvent::ModelDispatched(report) = &envelope.event {
            self.0.lock().unwrap().push(report.clone());
        }
    }
    async fn on_event(&self, _: &RunContext, _: &AgentEvent) {}
}
fn scope(
    connection: &str,
    models: Vec<ModelDescriptor>,
    provider: Arc<Capture>,
    primary: &str,
) -> ModelExecutionScope {
    let runtime = ModelRuntime::new(ModelRuntimeUpdate::new(
        connection,
        1,
        vec![ModelConnection::new(connection, provider, models).with_protocol("fixture.native")],
    ))
    .unwrap();
    ModelExecutionScope::new(
        runtime,
        Arc::new(FixedModelStrategy(ModelRef::new(connection, primary))),
    )
    .unwrap()
}
async fn agent(reports: Arc<Reports>) -> Agent {
    TestBuilder::default()
        .default_models(false)
        .plugin(ModelRuntimePlugin::new(
            ModelRuntime::new(ModelRuntimeUpdate::new("", 0, vec![])).unwrap(),
        ))
        .plugin(EventPlugin::from_arc("dispatch-observer", reports))
        .build()
        .await
        .unwrap()
}
#[tokio::test]
async fn scoped_runtime_is_authorized_and_selected_output_ceiling_precedes_assembly() {
    let reports = Arc::new(Reports::default());
    let agent = agent(reports.clone()).await;
    for (connection, ceiling, window) in [("alice", 16, 2048), ("bob", 8, 1024)] {
        let capture = Arc::new(Capture::default());
        let mut descriptor = ModelDescriptor::new("real-model");
        descriptor.context_window_tokens = Some(window);
        descriptor.max_output_tokens = Some(ceiling);
        let execution = scope(connection, vec![descriptor], capture.clone(), "real-model");
        let mut request = RunRequest::new(connection, "hello");
        request.generation.max_output_tokens = Some(128);
        request.limits.max_context_tokens = 8000;
        execution.attach(&mut request);
        assert_eq!(agent.run(request).await.unwrap().output, "real-model");
        let captured = capture.requests.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].1.options.max_output_tokens, Some(ceiling));
        let report = reports.0.lock().unwrap().last().unwrap().clone();
        assert_eq!(report.model, ModelRef::new(connection, "real-model"));
        assert_eq!(report.output_token_limit, ceiling);
        assert_eq!(report.input_capacity, window - ceiling as usize);
    }
    assert!(agent.close().await.is_success());
}
#[tokio::test]
async fn native_recovery_uses_real_authorized_candidate_with_its_smaller_window() {
    let reports = Arc::new(Reports::default());
    let agent = agent(reports.clone()).await;
    let capture = Arc::new(Capture::default());
    let mut narrow = ModelDescriptor::new("good");
    narrow.context_window_tokens = Some(1024);
    narrow.max_output_tokens = Some(8);
    let mut execution = scope(
        "authorized",
        vec![ModelDescriptor::new("removed"), narrow],
        capture.clone(),
        "removed",
    );
    execution.recovery.candidates = vec![ModelRef::new("authorized", "good")];
    let mut request = RunRequest::new("recovery", "hello");
    request.limits.max_model_retries = 1;
    request.limits.max_model_calls_per_step = Some(2);
    request.generation.max_output_tokens = Some(64);
    execution.attach(&mut request);
    assert_eq!(agent.run(request).await.unwrap().output, "good");
    assert_eq!(
        capture
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|v| v.0.as_str())
            .collect::<Vec<_>>(),
        vec!["removed", "good"]
    );
    let reports = reports.0.lock().unwrap().clone();
    assert_eq!(reports.len(), 2);
    assert_eq!(reports[1].model.model, "good");
    assert_eq!(reports[1].output_token_limit, 8);
    assert_eq!(reports[1].input_capacity, 1016);
    assert!(agent.close().await.is_success());
}
#[tokio::test]
async fn denied_tree_dispatch_releases_unspent_tokens_and_never_calls_provider() {
    let calls = Arc::new(AtomicUsize::new(0));
    let capture = Arc::new(Capture::default());
    let client = ModelJobClient::new().await.unwrap();
    let mut request = ChatRequest {
        messages: vec![Message::user("hello")],
        tools: vec![],
        extensions: Default::default(),
        options: Default::default(),
    };
    request
        .extensions
        .insert(ContinuationBudget::new(1, 4, 0, 1, 0, 0).unwrap());
    let job = ChatModelJob {
        connection: ModelConnection::new(
            "actual",
            capture.clone(),
            vec![ModelDescriptor::new("real")],
        ),
        model: ModelRef::new("actual", "real"),
        request,
        limits: RunLimits::default(),
        stage: "job".into(),
        max_calls: 1,
        repair: None,
        workflow: None,
    };
    let observed = client.execute_observed(job).await;
    assert!(
        observed
            .result
            .unwrap_err()
            .to_string()
            .contains("call budget")
    );
    assert_eq!(observed.report.calls, 0);
    assert_eq!(observed.report.ledger_usage, Usage::default());
    assert!(observed.report.provider_usage.is_none());
    assert_eq!(
        capture.requests.lock().unwrap().len(),
        calls.load(Ordering::SeqCst)
    );
    assert!(client.close_observed().await.is_success());
}
