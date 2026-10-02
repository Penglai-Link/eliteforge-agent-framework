use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_storage_local::{SqliteOptions, SqliteStateStore};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Notify;

#[derive(Clone, Default)]
struct Provider {
    calls: Arc<AtomicUsize>,
    entered: Arc<Notify>,
    stall: bool,
}
#[async_trait]
impl ChatModel for Provider {
    async fn generate(
        &self,
        _: &str,
        _: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        self.entered.notify_one();
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
fn job(provider: &Provider) -> ChatModelJob {
    ChatModelJob {
        workflow: None,
        connection: ModelConnection::new(
            "native",
            Arc::new(provider.clone()),
            vec![ModelDescriptor::new("actual")],
        ),
        model: ModelRef::new("native", "actual"),
        request: ChatRequest {
            messages: vec![Message::user("judge")],
            tools: vec![],
            options: GenerationOptions {
                max_output_tokens: Some(16),
                ..Default::default()
            },
            extensions: Default::default(),
        },
        limits: RunLimits {
            max_model_retries: 0,
            max_context_tokens: 1024,
            max_model_output_tokens: 16,
            ..Default::default()
        },
        stage: "goal_judge".into(),
        max_calls: 1,
        repair: None,
    }
}
struct Policy {
    client: ModelJobClient,
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
            .execute_child_observed(job(&self.provider), parent)
            .await
            .into_result()?;
        Ok(ContinuationDecision::Continue(json!({"approved":true})))
    }
    async fn execute(&self, _: Value, parent: &RunContext) -> Result<ContinuationStep, AgentError> {
        let response = self
            .client
            .execute_child_observed(job(&self.provider), parent)
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
    AgentBuilder::new()
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
async fn native_judge_and_child_share_original_calls_tokens_and_journal() {
    let temp = tempfile::tempdir().unwrap();
    let agent = driver(&temp.path().join("state.sqlite")).await;
    let policy = Arc::new(Policy {
        client: ModelJobClient::new().await.unwrap(),
        provider: Provider::default(),
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
    let journal: ContinuationJournalState = serde_json::from_value(checkpoint.state).unwrap();
    assert_eq!(journal.phase, ContinuationPhase::Exhausted);
    assert_eq!(journal.child_executions.len(), 2);
    assert_eq!(journal.model_calls, 4);
    assert_eq!(journal.budget_usage, observation.budget_usage);
    assert!(
        agent
            .run(request("same-identity", policy.clone()))
            .await
            .is_err()
    );
    assert_eq!(policy.provider.calls.load(Ordering::Acquire), 2);
    policy.client.close().await;
    agent.close().await;
}
#[tokio::test]
async fn cancelled_judge_retains_intent_and_refuses_automatic_replay() {
    let temp = tempfile::tempdir().unwrap();
    let agent = driver(&temp.path().join("state.sqlite")).await;
    let provider = Provider {
        stall: true,
        ..Default::default()
    };
    let entered = provider.entered.clone();
    let policy = Arc::new(Policy {
        client: ModelJobClient::new().await.unwrap(),
        provider,
        executed: AtomicUsize::new(0),
        paused: AtomicUsize::new(0),
    });
    let handle = agent.start(request("interrupted", policy.clone())).unwrap();
    let control = agent.control(&handle).unwrap();
    entered.notified().await;
    agent.cancel(&control).unwrap();
    let error = agent.wait(handle).await.unwrap_err();
    assert!(matches!(error.primary(), AgentError::Cancelled));
    let checkpoint = agent
        .latest_checkpoint("interrupted")
        .await
        .unwrap()
        .unwrap();
    let journal: ContinuationJournalState = serde_json::from_value(checkpoint.state).unwrap();
    assert_eq!(journal.phase, ContinuationPhase::Interrupted);
    assert_eq!(journal.child_executions.len(), 1);
    assert!(journal.budget_usage.input_tokens + journal.budget_usage.output_tokens > 10);
    assert!(
        agent
            .run(request("interrupted", policy.clone()))
            .await
            .is_err()
    );
    assert_eq!(policy.provider.calls.load(Ordering::Acquire), 1);
    assert_eq!(policy.executed.load(Ordering::Acquire), 0);
    policy.client.close().await;
    agent.close().await;
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
    let child = AgentBuilder::new()
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
    policy.child.close().await;
    agent.close().await;
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
    let agent = AgentBuilder::new()
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
    child.close().await;
    agent.close().await;
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
    let policy = Arc::new(Policy {
        client: ModelJobClient::new().await.unwrap(),
        provider: Provider::default(),
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
    agent.close().await;
}
#[tokio::test]
async fn initial_child_slots_are_not_refilled_before_first_judge() {
    let temp = tempfile::tempdir().unwrap();
    let agent = driver(&temp.path().join("state.sqlite")).await;
    let policy = Arc::new(Policy {
        client: ModelJobClient::new().await.unwrap(),
        provider: Provider::default(),
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
    agent.close().await;
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
    agent.close().await;
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
    root.close().await;
    child.close().await;
}
