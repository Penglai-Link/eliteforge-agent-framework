use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_model_http as http;
use eliteforge_agent_official_plugin_storage_local::{SqliteOptions, SqliteStateStore};
use eliteforge_agent_test_support::TestBuilder;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;

#[derive(Clone, Default)]
struct Provider {
    calls: Arc<AtomicUsize>,
    entered: Arc<Notify>,
    stall: bool,
    transport: Option<Arc<dyn ChatModel>>,
}
#[async_trait]
impl ChatModel for Provider {
    async fn generate(
        &self,
        model: &str,
        request: ChatRequest,
        stream: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        self.entered.notify_one();
        if let Some(transport) = &self.transport {
            return transport.generate(model, request, stream).await;
        }
        if self.stall {
            std::future::pending::<()>().await;
        }
        Ok(ChatResponse {
            content: "done".into(),
            usage: Usage {
                input_tokens: 4,
                output_tokens: 2,
            },
            ..Default::default()
        })
    }
}
struct ActiveHttp(Arc<AtomicUsize>);
impl Drop for ActiveHttp {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
struct JudgeHttp {
    adapter: Arc<dyn ChatModel>,
    calls: Arc<AtomicUsize>,
    active: Arc<AtomicUsize>,
    entered: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for JudgeHttp {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl JudgeHttp {
    async fn new(stall: bool) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(Notify::new());
        let (recorded, live, started) = (calls.clone(), active.clone(), entered.clone());
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let (recorded, live, started) = (recorded.clone(), live.clone(), started.clone());
                live.fetch_add(1, Ordering::AcqRel);
                tokio::spawn(async move {
                    let _active = ActiveHttp(live);
                    let mut bytes = vec![];
                    let mut buffer = [0; 4096];
                    loop {
                        let count = socket.read(&mut buffer).await.unwrap();
                        if count == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&buffer[..count]);
                        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
                        {
                            let headers = String::from_utf8_lossy(&bytes[..end]);
                            let length = headers
                                .lines()
                                .find_map(|line| {
                                    line.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|value| value.trim().parse::<usize>().unwrap())
                                })
                                .unwrap();
                            if bytes.len() >= end + 4 + length {
                                break;
                            }
                        }
                    }
                    recorded.fetch_add(1, Ordering::AcqRel);
                    started.notify_one();
                    if stall {
                        let _ = socket.read(&mut buffer).await;
                        return;
                    }
                    let body = json!({"choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],"usage":{"prompt_tokens":4,"completion_tokens":2}}).to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        let config = serde_json::from_value(json!({"modelCode":"actual","protocol":http::AgentModelProtocol::ChatCompletions,"baseUrl":url,"apiKey":"fixture","modelName":"actual"})).unwrap();
        let adapter = Arc::new(http::ProtocolChatModel {
            config,
            factory: Arc::new(http::FixedModelTransportFactory(Arc::new(
                http::HttpModelTransport::new(http::ModelHttpClient::new(), &url, "fixture"),
            ))),
            streaming: false,
        });
        Self {
            adapter,
            calls,
            active,
            entered,
            task,
        }
    }
    async fn drained(&self) {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while self.active.load(Ordering::Acquire) != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("direct HTTP connections must close before runtime teardown");
    }
}
fn model_client(provider: &Provider) -> ModelClient {
    ModelClient::new(ModelRuntimeUpdate::new(
        "native",
        1,
        vec![ModelConnection::new(
            "native",
            Arc::new(provider.clone()),
            vec![ModelDescriptor::new("actual")],
        )],
    ))
    .unwrap()
}
fn judge_input() -> ChatRequest {
    ChatRequest {
        messages: vec![Message::user("judge")],
        tools: vec![],
        extensions: Default::default(),
        options: GenerationOptions {
            max_output_tokens: Some(16),
            ..Default::default()
        },
    }
}
fn judge_limits() -> RunLimits {
    RunLimits {
        max_model_retries: 0,
        max_context_tokens: 1024,
        max_model_output_tokens: 16,
        max_model_calls_per_step: Some(1),
        ..Default::default()
    }
}
struct Policy {
    client: ModelClient,
    provider: Provider,
    executed: AtomicUsize,
    paused: AtomicUsize,
}
#[async_trait]
impl ContinuationPolicy for Policy {
    async fn evaluate(
        &self,
        _: &Value,
        parent: &RunContext,
    ) -> Result<ContinuationDecision, AgentError> {
        self.client
            .with_options(ModelInvocationOptions::for_run(parent, &judge_limits()))
            .chat_observed(
                &ModelRef::new("native", "actual"),
                judge_input(),
                &judge_limits(),
                "goal_judge",
            )
            .await
            .into_result()?;
        Ok(ContinuationDecision::Continue(json!({"approved":true})))
    }
    async fn execute(&self, _: Value, parent: &RunContext) -> Result<ContinuationStep, AgentError> {
        let response = self
            .client
            .with_options(ModelInvocationOptions::for_run(parent, &judge_limits()))
            .chat_observed(
                &ModelRef::new("native", "actual"),
                judge_input(),
                &judge_limits(),
                "goal_judge",
            )
            .await
            .into_result()?;
        self.executed.fetch_add(1, Ordering::AcqRel);
        Ok(ContinuationStep {
            state: json!({"complete":true}),
            content: response.content,
            interactions: vec![],
            status: RunStatus::Completed,
        })
    }
    async fn budget_exhausted(&self, _: &Value, _: &RunContext) -> Result<(), AgentError> {
        self.paused.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}
async fn driver(path: &std::path::Path) -> Agent {
    let store = Arc::new(
        SqliteStateStore::open(path, SqliteOptions::default())
            .await
            .unwrap(),
    );
    TestBuilder::new()
        .default_models(false)
        .plugin(StateStorePlugin(store))
        .plugin(AgentLoopPlugin::new(ContinuationLoop))
        .build()
        .await
        .unwrap()
}
fn snapshot() -> RunExecutionSnapshot {
    RunExecutionSnapshot {
        limits: RunLimits {
            max_steps: 4,
            max_model_calls_per_step: Some(1),
            max_total_tokens: 100,
            max_model_output_tokens: 16,
            max_context_tokens: 1024,
            ..Default::default()
        },
        budget_usage: Usage {
            input_tokens: 10,
            output_tokens: 0,
        },
        model_calls: 2,
        steps: 1,
        tool_calls: 0,
        deadline_unix_ms: unix_time_ms() + 5000,
        opaque: false,
        incomplete: false,
        historical_calls_bound: None,
        child_runs: 0,
    }
}
fn request(session: &str, policy: Arc<dyn ContinuationPolicy>) -> RunRequest {
    let mut run = RunRequest::new(session, "");
    ContinuationRequest {
        policy,
        initial_state: json!({"runCode":"initial"}),
        initial_execution: snapshot(),
        max_rounds: 20,
    }
    .attach(&mut run)
    .unwrap();
    run
}
#[tokio::test]
async fn direct_judge_and_step_share_original_calls_tokens_and_durable_journal() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state.sqlite");
    let agent = driver(&path).await;
    let http = JudgeHttp::new(false).await;
    let provider = Provider {
        transport: Some(http.adapter.clone()),
        ..Default::default()
    };
    let policy = Arc::new(Policy {
        client: model_client(&provider),
        provider,
        executed: AtomicUsize::new(0),
        paused: AtomicUsize::new(0),
    });
    let handle = agent
        .start(request("same-identity", policy.clone()))
        .unwrap();
    let control = agent.control(&handle).unwrap();
    let result = agent.wait(handle).await.unwrap();
    let observation = agent.execution_snapshot(&control, &result).unwrap();
    assert_eq!(result.status, RunStatus::StepLimit);
    assert_eq!(policy.provider.calls.load(Ordering::Acquire), 2);
    assert_eq!(policy.executed.load(Ordering::Acquire), 1);
    assert_eq!(policy.paused.load(Ordering::Acquire), 1);
    assert_eq!(observation.model_calls, 4);
    assert_eq!(
        observation.budget_usage,
        Usage {
            input_tokens: 18,
            output_tokens: 4
        }
    );
    let checkpoint = agent
        .latest_checkpoint("same-identity")
        .await
        .unwrap()
        .unwrap();
    let journal: ContinuationJournalState =
        serde_json::from_value(checkpoint.state.clone()).unwrap();
    assert_eq!(journal.phase, ContinuationPhase::Exhausted);
    assert!(journal.child_executions.is_empty());
    assert_eq!(journal.child_runs, 0);
    assert_eq!(observation.child_runs, 0);
    assert_eq!(journal.rounds, 1);
    assert_eq!(journal.state, json!({"complete":true}));
    assert!(journal.pending.is_none());
    assert_eq!(journal.model_calls, 4);
    assert_eq!(journal.budget_usage, observation.budget_usage);
    assert!(
        agent
            .run(request("same-identity", policy.clone()))
            .await
            .is_err()
    );
    assert_eq!(policy.provider.calls.load(Ordering::Acquire), 2);
    assert_eq!(http.calls.load(Ordering::Acquire), 2);
    agent.framework().close().await;
    let reopened = driver(&path).await;
    let error = reopened
        .run(request("same-identity", policy.clone()))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("already admitted"));
    let unchanged = reopened
        .latest_checkpoint("same-identity")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.run_id, checkpoint.run_id);
    assert_eq!(unchanged.revision, checkpoint.revision);
    assert_eq!(unchanged.state, checkpoint.state);
    assert_eq!(http.calls.load(Ordering::Acquire), 2);
    assert_eq!(policy.provider.calls.load(Ordering::Acquire), 2);
    http.drained().await;
    policy.client.close().await;
    reopened.close().await;
}
#[tokio::test]
async fn cancelled_judge_retains_intent_and_refuses_automatic_replay() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state.sqlite");
    let agent = driver(&path).await;
    let http = JudgeHttp::new(true).await;
    let provider = Provider {
        transport: Some(http.adapter.clone()),
        ..Default::default()
    };
    let entered = http.entered.clone();
    let policy = Arc::new(Policy {
        client: model_client(&provider),
        provider,
        executed: AtomicUsize::new(0),
        paused: AtomicUsize::new(0),
    });
    let handle = agent.start(request("interrupted", policy.clone())).unwrap();
    let control = agent.control(&handle).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), entered.notified())
        .await
        .expect("actual HTTP request must arrive before cancellation");
    // The real HTTP request has arrived. Its intent and reservation must already
    // exist durably, even while the response is still outstanding.
    let admitted = agent
        .latest_checkpoint("interrupted")
        .await
        .unwrap()
        .unwrap();
    let intent: ContinuationJournalState = serde_json::from_value(admitted.state).unwrap();
    assert_eq!(intent.phase, ContinuationPhase::Evaluating);
    assert_eq!(intent.model_calls, 3);
    assert_eq!(intent.rounds, 0);
    assert_eq!(intent.state, json!({"runCode":"initial"}));
    assert!(intent.pending.is_none());
    assert!(intent.failure.is_none());
    assert!(intent.child_executions.is_empty());
    assert_eq!(intent.child_runs, 0);
    assert!(intent.budget_usage.input_tokens + intent.budget_usage.output_tokens > 10);
    assert_eq!(http.calls.load(Ordering::Acquire), 1);
    agent.cancel(&control).unwrap();
    let error = agent.wait(handle).await.unwrap_err();
    assert!(matches!(error.primary(), AgentError::Cancelled));
    let checkpoint = agent
        .latest_checkpoint("interrupted")
        .await
        .unwrap()
        .unwrap();
    let journal: ContinuationJournalState =
        serde_json::from_value(checkpoint.state.clone()).unwrap();
    assert_eq!(journal.phase, ContinuationPhase::Interrupted);
    assert!(journal.child_executions.is_empty());
    assert_eq!(journal.child_runs, 0);
    assert_eq!(journal.model_calls, 3);
    assert_eq!(journal.rounds, 0);
    assert_eq!(journal.state, intent.state);
    assert!(journal.pending.is_none());
    assert!(journal.failure.is_some());
    assert!(journal.budget_usage.input_tokens + journal.budget_usage.output_tokens > 10);
    assert!(
        agent
            .run(request("interrupted", policy.clone()))
            .await
            .is_err()
    );
    assert_eq!(policy.provider.calls.load(Ordering::Acquire), 1);
    assert_eq!(policy.executed.load(Ordering::Acquire), 0);
    agent.framework().close().await;
    let reopened = driver(&path).await;
    let error = reopened
        .run(request("interrupted", policy.clone()))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("already admitted"));
    let unchanged = reopened
        .latest_checkpoint("interrupted")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.run_id, checkpoint.run_id);
    assert_eq!(unchanged.revision, checkpoint.revision);
    assert_eq!(unchanged.state, checkpoint.state);
    assert_eq!(http.calls.load(Ordering::Acquire), 1);
    assert_eq!(policy.provider.calls.load(Ordering::Acquire), 1);
    assert_eq!(policy.executed.load(Ordering::Acquire), 0);
    http.drained().await;
    policy.client.close().await;
    reopened.close().await;
}
struct Opaque;
#[async_trait]
impl ExternalAgent for Opaque {
    async fn run(
        &self,
        _: ExternalAgentRequest,
        _: &dyn ExternalAgentEventSink,
    ) -> Result<ExternalAgentResult, AgentError> {
        Ok(ExternalAgentResult {
            content: "remote".into(),
            structured: None,
            session: None,
            usage: Usage::default(),
        })
    }
}
struct OpaquePolicy {
    child: Agent,
    executed: AtomicUsize,
    paused: AtomicUsize,
}
#[async_trait]
impl ContinuationPolicy for OpaquePolicy {
    async fn evaluate(
        &self,
        _: &Value,
        parent: &RunContext,
    ) -> Result<ContinuationDecision, AgentError> {
        let result = self
            .child
            .run_child(RunRequest::new("opaque-child", "work"), parent)
            .await?;
        assert_eq!(result.usage, Usage::default());
        Ok(ContinuationDecision::Continue(Value::Null))
    }
    async fn execute(&self, _: Value, _: &RunContext) -> Result<ContinuationStep, AgentError> {
        self.executed.fetch_add(1, Ordering::AcqRel);
        unreachable!("unknown remote usage consumes its whole allowance")
    }
    async fn budget_exhausted(&self, _: &Value, _: &RunContext) -> Result<(), AgentError> {
        self.paused.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}
#[tokio::test]
async fn opaque_unknown_usage_is_charged_without_inventing_reported_usage() {
    let temp = tempfile::tempdir().unwrap();
    let agent = driver(&temp.path().join("state.sqlite")).await;
    let child = TestBuilder::new()
        .default_models(false)
        .plugin(ExternalAgentPlugin::new(Opaque))
        .plugin(AgentLoopPlugin::new(ExternalAgentLoop))
        .build()
        .await
        .unwrap();
    let policy = Arc::new(OpaquePolicy {
        child,
        executed: AtomicUsize::new(0),
        paused: AtomicUsize::new(0),
    });
    let handle = agent.start(request("opaque", policy.clone())).unwrap();
    let control = agent.control(&handle).unwrap();
    let result = agent.wait(handle).await.unwrap();
    let observation = agent.execution_snapshot(&control, &result).unwrap();
    assert!(observation.opaque);
    assert_eq!(observation.model_calls, 3);
    assert_eq!(
        observation.budget_usage.input_tokens + observation.budget_usage.output_tokens,
        100
    );
    assert_eq!(policy.executed.load(Ordering::Acquire), 0);
    assert_eq!(policy.paused.load(Ordering::Acquire), 1);
    policy.child.framework().close().await;
    agent.framework().close().await;
}
#[test]
fn continuation_does_not_reset_expired_deadline_or_remove_explicit_call_ceiling() {
    let mut initial = snapshot();
    initial.deadline_unix_ms = unix_time_ms().saturating_sub(1);
    struct Never;
    #[async_trait]
    impl ContinuationPolicy for Never {
        async fn evaluate(
            &self,
            _: &Value,
            _: &RunContext,
        ) -> Result<ContinuationDecision, AgentError> {
            unreachable!()
        }
        async fn execute(&self, _: Value, _: &RunContext) -> Result<ContinuationStep, AgentError> {
            unreachable!()
        }
        async fn budget_exhausted(&self, _: &Value, _: &RunContext) -> Result<(), AgentError> {
            unreachable!()
        }
    }
    let mut run = RunRequest::new("expired", "");
    assert!(
        ContinuationRequest {
            policy: Arc::new(Never),
            initial_state: Value::Null,
            initial_execution: initial,
            max_rounds: 20
        }
        .attach(&mut run)
        .is_err()
    );
    let mut initial = snapshot();
    initial.limits.max_model_calls_per_step = None;
    assert!(
        ContinuationRequest {
            policy: Arc::new(Never),
            initial_state: Value::Null,
            initial_execution: initial,
            max_rounds: 1
        }
        .attach(&mut run)
        .is_err()
    );
}

struct InitialDelegator {
    child: Agent,
}
#[async_trait]
impl AgentLoop for InitialDelegator {
    fn requires_conversations(&self) -> bool {
        false
    }
    async fn run(
        &self,
        _: &RunRequest,
        parent: &RunContext,
        _: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        for index in 0..2 {
            let mut request = RunRequest::new(format!("initial-child-{index}"), "work");
            request.limits.max_steps = 1;
            self.child.run_child(request, parent).await?;
        }
        Ok(RunResult {
            output: "root".into(),
            content: "root".into(),
            structured: None,
            interactions: vec![],
            status: RunStatus::Completed,
            usage: parent.usage(),
            steps: 0,
            tool_calls: 0,
            external_session: None,
        })
    }
}
#[tokio::test]
async fn initial_snapshot_observes_native_delegated_steps_before_goal_driver() {
    let child = eliteforge_agent_test_support::TestBuilder::default()
        .default_models(false)
        .plugin(ChatModelPlugin::new(Provider::default()))
        .build()
        .await
        .unwrap();
    let agent = TestBuilder::new()
        .default_models(false)
        .plugin(AgentLoopPlugin::new(InitialDelegator {
            child: child.clone(),
        }))
        .build()
        .await
        .unwrap();
    let handle = agent
        .start(RunRequest::new("initial-root", "work"))
        .unwrap();
    let control = agent.control(&handle).unwrap();
    let result = agent.wait(handle).await.unwrap();
    assert_eq!(result.steps, 0);
    let observation = agent.execution_snapshot(&control, &result).unwrap();
    assert_eq!(observation.steps, 2);
    assert_eq!(observation.model_calls, 2);
    assert_eq!(observation.child_runs, 2);
    assert_eq!(
        observation.budget_usage,
        Usage {
            input_tokens: 8,
            output_tokens: 4
        }
    );
    child.framework().close().await;
    agent.framework().close().await;
}
#[test]
fn global_counters_preserve_child_local_caps_and_do_not_use_child_cap_as_root_cap() {
    use eliteforge_agent_core::budget::ExecutionCounters;
    let budget = ContinuationBudget::new(8, 4, 4, 2, 2, 0).unwrap();
    let shared = budget.execution_counters();
    // A one-step child can run after two previous root steps.
    assert!(budget.admit_step(Some(&shared), 1));
    assert_eq!(budget.steps(), 3);
    let local = Arc::new(ExecutionCounters {
        steps: AtomicUsize::new(0),
        tools: AtomicUsize::new(0),
    });
    assert!(budget.admit_step(Some(&local), 1));
    assert_eq!(budget.steps(), 4);
    assert!(!budget.admit_step(Some(&local), 1));
    assert_eq!(local.steps.load(Ordering::Acquire), 1);
    assert!(budget.admit_tools(Some(&local), 2, 2));
    assert!(!budget.admit_tools(Some(&local), 2, 1));
    assert_eq!(budget.tool_calls(), 2);
    let other = Arc::new(ExecutionCounters {
        steps: AtomicUsize::new(0),
        tools: AtomicUsize::new(0),
    });
    assert!(!budget.admit_step(Some(&other), 3));
    assert_eq!(other.steps.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn expired_attach_to_start_wait_cannot_extend_original_deadline() {
    let temp = tempfile::tempdir().unwrap();
    let agent = driver(&temp.path().join("state.sqlite")).await;
    let provider = Provider::default();
    let policy = Arc::new(Policy {
        client: model_client(&provider),
        provider,
        executed: AtomicUsize::new(0),
        paused: AtomicUsize::new(0),
    });
    let mut initial = snapshot();
    initial.deadline_unix_ms = unix_time_ms() + 20;
    let mut run = RunRequest::new("late-start", "");
    ContinuationRequest {
        policy: policy.clone(),
        initial_state: Value::Null,
        initial_execution: initial,
        max_rounds: 20,
    }
    .attach(&mut run)
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert!(matches!(
        agent.start(run).err().unwrap().primary(),
        AgentError::Cancelled
    ));
    assert_eq!(policy.provider.calls.load(Ordering::Acquire), 0);
    policy.client.close().await;
    agent.framework().close().await;
}
#[tokio::test]
async fn initial_child_slots_are_not_refilled_before_first_judge() {
    let temp = tempfile::tempdir().unwrap();
    let agent = driver(&temp.path().join("state.sqlite")).await;
    let provider = Provider::default();
    let policy = Arc::new(Policy {
        client: model_client(&provider),
        provider,
        executed: AtomicUsize::new(0),
        paused: AtomicUsize::new(0),
    });
    let mut initial = snapshot();
    initial.limits.max_child_runs = 2;
    initial.child_runs = 2;
    let mut run = RunRequest::new("no-child-slots", "");
    ContinuationRequest {
        policy: policy.clone(),
        initial_state: Value::Null,
        initial_execution: initial,
        max_rounds: 20,
    }
    .attach(&mut run)
    .unwrap();
    let result = agent.run(run).await.unwrap();
    assert_eq!(result.status, RunStatus::StepLimit);
    assert_eq!(policy.provider.calls.load(Ordering::Acquire), 0);
    assert_eq!(policy.paused.load(Ordering::Acquire), 1);
    let state: ContinuationJournalState = serde_json::from_value(
        agent
            .latest_checkpoint("no-child-slots")
            .await
            .unwrap()
            .unwrap()
            .state,
    )
    .unwrap();
    assert_eq!(state.child_runs, 2);
    assert!(state.child_executions.is_empty());
    policy.client.close().await;
    agent.framework().close().await;
}
#[tokio::test]
async fn resumed_native_checkpoint_charges_saved_step_call_bound_with_original_limits() {
    let provider = Provider::default();
    let definition = WorkflowDefinition {
        id: "two-stages".into(),
        version: 1,
        stages: vec![WorkflowStage::new("first"), WorkflowStage::new("second")],
    };
    let agent = eliteforge_agent_test_support::TestBuilder::default()
        .default_models(false)
        .plugin(ChatModelPlugin::new(provider.clone()))
        .plugin(AgentLoopPlugin::new(WorkflowLoop::new(definition).unwrap()))
        .build()
        .await
        .unwrap();
    let mut run = RunRequest::new("resume-budget", "work");
    run.limits.max_steps = 4;
    run.limits.max_model_calls_per_step = Some(3);
    run.limits.max_model_output_tokens = 16;
    let handle = agent.start(run).unwrap();
    let id = handle.run_id;
    let result = agent.wait(handle).await.unwrap();
    assert_eq!(result.steps, 2);
    let mut resumed = RunRequest::new("resume-budget", "");
    resumed.resume = Some(ResumeRequest {
        run_id: id,
        response: None,
    });
    let handle = agent.start(resumed).unwrap();
    let control = agent.control(&handle).unwrap();
    let result = agent.wait(handle).await.unwrap();
    let observed = agent.execution_snapshot(&control, &result).unwrap();
    assert!(observed.incomplete);
    assert_eq!(observed.historical_calls_bound, Some(6));
    assert_eq!(observed.model_calls, 6);
    assert_eq!(observed.limits.max_steps, 4);
    assert_eq!(observed.limits.max_model_calls_per_step, Some(3));
    assert_eq!(
        observed.budget_usage,
        Usage {
            input_tokens: 8,
            output_tokens: 4
        }
    );
    assert_eq!(provider.calls.load(Ordering::Acquire), 2);
    agent.framework().close().await;
}
#[test]
fn exhausted_usize_max_dispatch_bound_rejects_without_overflow() {
    let budget = ContinuationBudget::new(usize::MAX, 1, 1, usize::MAX, 0, 0).unwrap();
    assert!(budget.admit_model_call().is_err());
}

struct ChildLimitEcho;
#[async_trait]
impl AgentLoop for ChildLimitEcho {
    fn requires_conversations(&self) -> bool {
        false
    }
    async fn run(
        &self,
        _: &RunRequest,
        context: &RunContext,
        _: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let output = context
            .limits()
            .max_model_calls_per_step
            .expect("inherited explicit ceiling")
            .to_string();
        Ok(RunResult {
            content: output.clone().into(),
            output,
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
struct ChildLimitDelegator {
    child: Agent,
}
#[async_trait]
impl AgentLoop for ChildLimitDelegator {
    fn requires_conversations(&self) -> bool {
        false
    }
    async fn run(
        &self,
        _: &RunRequest,
        parent: &RunContext,
        _: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let mut observed = vec![];
        for (index, local) in [None, Some(5), Some(1)].into_iter().enumerate() {
            let mut request = RunRequest::new(format!("limited-child-{index}"), "");
            request.limits.max_steps = 1;
            request.limits.max_model_calls_per_step = local;
            let result = self.child.run_child(request, parent).await?;
            observed.push(result.output.parse::<usize>().unwrap());
        }
        let output = serde_json::to_string(&observed).unwrap();
        Ok(RunResult {
            content: output.clone().into(),
            output,
            structured: None,
            interactions: vec![],
            status: RunStatus::Completed,
            usage: parent.usage(),
            steps: 0,
            tool_calls: 0,
            external_session: None,
        })
    }
}
#[tokio::test]
async fn native_children_preserve_ancestor_per_step_cap_and_tighter_local_caps() {
    let child = eliteforge_agent_test_support::TestBuilder::default()
        .default_models(false)
        .plugin(AgentLoopPlugin::new(ChildLimitEcho))
        .build()
        .await
        .unwrap();
    let root = eliteforge_agent_test_support::TestBuilder::default()
        .default_models(false)
        .plugin(AgentLoopPlugin::new(ChildLimitDelegator {
            child: child.clone(),
        }))
        .build()
        .await
        .unwrap();
    let budget = ContinuationBudget::new(16, 8, 32, 0, 0, 0).unwrap();
    let mut request = RunRequest::new("inherited-per-step", "");
    request.limits.max_steps = 8;
    request.limits.max_model_calls_per_step = Some(2);
    request.extensions.insert(budget.clone());
    let result = root.run(request).await.unwrap();
    assert_eq!(result.output, "[2,2,1]");
    assert_eq!(budget.calls(), 0);
    for _ in 0..16 {
        budget.admit_model_call().unwrap();
    }
    assert!(budget.admit_model_call().is_err());
    root.framework().close().await;
    child.framework().close().await;
}
