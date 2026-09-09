use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_state_sqlite::*;
use eliteforge_agent_test_support::*;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

fn definition() -> WorkflowDefinition {
    WorkflowDefinition {
        id: "arithmetic".into(),
        version: 1,
        stages: vec![WorkflowStage::new("solve")],
    }
}
fn resume(session: &str, id: RunId, response: Option<InteractionResponse>) -> RunRequest {
    let mut request = RunRequest::new(session, "");
    request.resume = Some(ResumeRequest {
        run_id: id,
        response,
        resolutions: Vec::new(),
    });
    request
}
async fn database(path: &std::path::Path) -> Arc<SqliteStateStore> {
    Arc::new(
        SqliteStateStore::open(path, SqliteOptions::default())
            .await
            .unwrap(),
    )
}

#[derive(Clone, Default)]
struct SteeringModel {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
}
#[async_trait]
impl ChatModel for SteeringModel {
    async fn generate(
        &self,
        _model_id: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        let first = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request);
            requests.len() == 1
        };
        if first {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(ChatResponse::text(if first { "draft" } else { "done" }))
    }
}

async fn steering_agent(store: Arc<dyn StateStore>, model: SteeringModel) -> Agent {
    TestBuilder::default()
        .plugin(StateStorePlugin(store))
        .plugin(ChatModelPlugin::new(model))
        .plugin(AgentLoopPlugin::new(
            WorkflowLoop::new(definition()).unwrap(),
        ))
        .build()
        .await
        .unwrap()
}

#[tokio::test]
async fn final_reply_steering_records_ordered_live_and_durable_events_without_replay_duplicates() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("steering.db");
    let store = database(&path).await;
    let model = SteeringModel::default();
    let agent = steering_agent(store.clone(), model.clone()).await;
    let mut handle = agent.start(RunRequest::new("steering", "task")).unwrap();
    let control = handle.control();
    let mut events = handle.take_events();
    let waiting = tokio::spawn(handle.wait());
    tokio::time::timeout(Duration::from_secs(5), model.entered.notified())
        .await
        .unwrap();
    control.steer("include tests").unwrap();
    control.clone().steer("explain the result").unwrap();
    model.release.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result.status, RunStatus::Completed);
    assert_eq!(result.steps, 2);
    assert!(control.steer("after completion").is_err());
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1]
                .messages
                .iter()
                .map(|m| (m.role, m.content.text()))
                .collect::<Vec<_>>(),
            [
                (Role::User, "task".into()),
                (Role::Assistant, "draft".into()),
                (Role::User, "include tests".into()),
                (Role::User, "explain the result".into()),
            ]
        );
    }
    let mut live = Vec::new();
    while let Ok(envelope) = events.try_recv() {
        if let AgentEvent::Steered(text) = envelope.event {
            assert!(matches!(envelope.scope, EventScope::Live { .. }));
            live.push(text);
        }
    }
    assert_eq!(live, ["include tests", "explain the result"]);
    let saved = store.load(control.run_id()).await.unwrap();
    let journal: Vec<_> = saved
        .events
        .iter()
        .filter_map(|envelope| {
            if let AgentEvent::Steered(text) = &envelope.event {
                assert_eq!(envelope.scope, EventScope::Journal);
                Some(text.clone())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(journal, live);
    validate_messages(&saved.transcript).unwrap();
    agent.close().await;
    store.close().await.unwrap();

    let reopened = database(&path).await;
    let agent = steering_agent(reopened.clone(), model.clone()).await;
    let mut replay = agent
        .start(resume("steering", control.run_id(), None))
        .unwrap();
    let mut replay_events = replay.take_events();
    // Even a resumed execution with the same RunId has separate controls.
    control.cancel();
    assert_eq!(replay.wait().await.unwrap().output, "done");
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    while let Ok(envelope) = replay_events.try_recv() {
        assert!(!matches!(envelope.event, AgentEvent::Steered(_)));
    }
    let after = reopened.load(control.run_id()).await.unwrap();
    assert_eq!(
        serde_json::to_value(&after.events).unwrap(),
        serde_json::to_value(&saved.events).unwrap()
    );
    assert_eq!(after.transcript, saved.transcript);
    agent.close().await;
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn failed_final_reply_steering_save_does_not_publish_a_steered_event() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("steering-failure.db");
    let store = database(&path).await;
    let model = SteeringModel::default();
    let fault = Arc::new(CleanupFaultStore {
        inner: store.clone(),
        fail_acquired_load: false,
        lease: Mutex::new(None),
        release_fault: ReleaseFault::SteeringCommit,
        release_calls: AtomicUsize::new(0),
    });
    let agent = steering_agent(fault, model.clone()).await;
    let mut handle = agent.start(RunRequest::new("steering", "task")).unwrap();
    let control = handle.control();
    let mut events = handle.take_events();
    let waiting = tokio::spawn(handle.wait());
    tokio::time::timeout(Duration::from_secs(5), model.entered.notified())
        .await
        .unwrap();
    control.steer("include tests").unwrap();
    // Only the steering commit fails; model budget settlement still succeeds.
    model.release.notify_one();
    let error = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(
        matches!(error.primary(), AgentError::State(StateError::Backend(message))
            if message == "steering checkpoint write failed"),
        "{error}"
    );
    assert!(control.steer("after failure").is_err());
    while let Ok(envelope) = events.try_recv() {
        assert!(!matches!(envelope.event, AgentEvent::Steered(_)));
    }
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    agent.close().await;
    store.close().await.unwrap();
    let reopened = database(&path).await;
    let saved = reopened.load(control.run_id()).await.unwrap();
    assert!(
        saved
            .events
            .iter()
            .all(|e| !matches!(e.event, AgentEvent::Steered(_)))
    );
    assert!(
        saved
            .transcript
            .iter()
            .all(|m| m.content.text() != "include tests")
    );
    reopened.close().await.unwrap();
}

#[derive(Clone)]
struct Arithmetic {
    calls: Arc<AtomicUsize>,
}
#[async_trait]
impl ChatModel for Arithmetic {
    async fn generate(
        &self,
        _model_id: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(result) = request.messages.iter().rev().find(|m| m.role == Role::Tool) {
            Ok(ChatResponse::text(result.content.protocol_text()))
        } else {
            let amount = request
                .messages
                .iter()
                .rev()
                .find(|m| m.role == Role::User)
                .unwrap()
                .content
                .text()
                .parse::<i64>()
                .unwrap();
            Ok(ChatResponse::tool_calls(vec![ToolCall::new(
                "provider-call",
                "double",
                json!({"amount":amount}),
            )]))
        }
    }
}
struct Double {
    effects: Arc<AtomicUsize>,
}
#[async_trait]
impl Tool for Double {
    async fn call(&self, args: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        self.effects.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::text(
            (args["amount"].as_i64().unwrap() * 2).to_string(),
        ))
    }
}
fn double_tool(effects: Arc<AtomicUsize>) -> ToolBinding {
    ToolBinding::new(
        ToolDescriptor::new(
            "math.double",
            "double",
            "Double amount",
            json!({"type":"object","properties":{"amount":{"type":"integer"}},"required":["amount"],"additionalProperties":false}),
        ),
        Double { effects },
    )
}
struct Approval;
#[async_trait]
impl ToolPolicy for Approval {
    async fn evaluate(
        &self,
        invocation: &ToolInvocationContext<'_>,
    ) -> Result<ToolPolicyDecision, AgentError> {
        Ok(ToolPolicyDecision::RequireApproval(
            format!(
                "Execute {} with {}?",
                invocation.descriptor.id, invocation.call.arguments
            )
            .into(),
        ))
    }
}
async fn arithmetic_agent(
    store: Arc<SqliteStateStore>,
    calls: Arc<AtomicUsize>,
    effects: Arc<AtomicUsize>,
) -> Agent {
    TestBuilder::default()
        .plugin(store.plugin())
        .plugin(ChatModelPlugin::new(Arithmetic { calls }))
        .plugin(AgentLoopPlugin::new(
            WorkflowLoop::new(definition()).unwrap(),
        ))
        .plugin(ToolPolicyPlugin::new("approval", Approval))
        .plugin(ToolProviderPlugin::new(
            "tools",
            StaticToolProvider::new(vec![double_tool(effects)]),
        ))
        .build()
        .await
        .unwrap()
}

#[tokio::test]
async fn approval_survives_restart_and_duplicate_answer_replays_without_execution() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("approval.db");
    let calls = Arc::new(AtomicUsize::new(0));
    let effects = Arc::new(AtomicUsize::new(0));
    let store = database(&path).await;
    let agent = arithmetic_agent(store.clone(), calls.clone(), effects.clone()).await;
    let handle = agent.start(RunRequest::new("approved", "17")).unwrap();
    let run_id = handle.run_id;
    let waiting = handle.wait().await.unwrap();
    assert_eq!(waiting.status, RunStatus::AwaitingInput);
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    let question = &waiting.interactions[0];
    let saved = store.load(run_id).await.unwrap();
    let state: WorkflowState = serde_json::from_value(saved.state).unwrap();
    let WorkflowCursor::Tools { executions, .. } = state.cursor else {
        panic!("missing tools")
    };
    assert_eq!(executions[0].call.arguments, json!({"amount":17}));
    assert_eq!(
        question.kind,
        InteractionKind::Approval {
            invocation_id: executions[0].invocation_id
        }
    );
    agent.close().await;
    store.close().await.unwrap();
    let store = database(&path).await;
    let agent = arithmetic_agent(store.clone(), calls.clone(), effects.clone()).await;
    assert!(
        agent
            .run(resume(
                "approved",
                run_id,
                Some(InteractionResponse {
                    interaction_id: RunId::new_v4(),
                    value: json!(true)
                })
            ))
            .await
            .is_err()
    );
    assert_eq!(
        store.load(run_id).await.unwrap().status,
        ExecutionStatus::Waiting
    );
    let answer = InteractionResponse {
        interaction_id: question.id,
        value: json!(true),
    };
    let finished = agent
        .run(resume("approved", run_id, Some(answer.clone())))
        .await
        .unwrap();
    assert_eq!(finished.output, "34");
    assert_eq!(finished.steps, 2);
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let duplicate = agent
        .run(resume("approved", run_id, Some(answer)))
        .await
        .unwrap();
    assert_eq!(duplicate.output, "34");
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(
        agent
            .run(resume(
                "approved",
                run_id,
                Some(InteractionResponse {
                    interaction_id: question.id,
                    value: json!(false)
                })
            ))
            .await
            .is_err()
    );
    validate_messages(&store.load(run_id).await.unwrap().transcript).unwrap();
    agent.close().await;
    store.close().await.unwrap();
}

struct EffectThenHang {
    effects: Arc<AtomicUsize>,
    started: Arc<tokio::sync::Notify>,
}
#[async_trait]
impl Tool for EffectThenHang {
    async fn call(&self, _: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        self.effects.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        std::future::pending().await
    }
}
#[tokio::test]
async fn cancellation_after_side_effect_requires_reconciliation_and_never_repeats_it() {
    let directory = tempfile::tempdir().unwrap();
    let store = database(&directory.path().join("uncertain.db")).await;
    let effects = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(tokio::sync::Notify::new());
    let descriptor = double_tool(effects.clone()).descriptor;
    let agent = TestBuilder::default()
        .plugin(store.clone().plugin())
        .plugin(ChatModelPlugin::new(Arithmetic {
            calls: calls.clone(),
        }))
        .plugin(AgentLoopPlugin::new(
            WorkflowLoop::new(definition()).unwrap(),
        ))
        .plugin(ToolProviderPlugin::new(
            "tools",
            StaticToolProvider::new(vec![ToolBinding::new(
                descriptor.clone(),
                EffectThenHang {
                    effects: effects.clone(),
                    started: started.clone(),
                },
            )]),
        ))
        .build()
        .await
        .unwrap();
    let handle = agent.start(RunRequest::new("uncertain", "9")).unwrap();
    let run_id = handle.run_id;
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    handle.cancel();
    let stopped = handle.wait().await.unwrap();
    assert_eq!(stopped.status, RunStatus::RecoveryRequired);
    agent.close().await;
    let agent = TestBuilder::default()
        .plugin(store.clone().plugin())
        .plugin(ChatModelPlugin::new(Arithmetic {
            calls: calls.clone(),
        }))
        .plugin(AgentLoopPlugin::new(
            WorkflowLoop::new(definition()).unwrap(),
        ))
        .plugin(ToolProviderPlugin::new(
            "tools",
            StaticToolProvider::new(vec![ToolBinding::new(
                descriptor,
                Double {
                    effects: effects.clone(),
                },
            )]),
        ))
        .build()
        .await
        .unwrap();
    let stopped = agent.run(resume("uncertain", run_id, None)).await.unwrap();
    assert_eq!(stopped.status, RunStatus::RecoveryRequired);
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    let state: WorkflowState =
        serde_json::from_value(store.load(run_id).await.unwrap().state).unwrap();
    let WorkflowCursor::Tools { executions, .. } = state.cursor else {
        panic!("missing journal")
    };
    let mut request = resume("uncertain", run_id, None);
    request
        .resume
        .as_mut()
        .unwrap()
        .resolutions
        .push(CallResolution::Record {
            invocation_id: executions[0].invocation_id,
            output: ToolOutput::text("18"),
        });
    assert_eq!(agent.run(request).await.unwrap().output, "18");
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    agent.close().await;
    store.close().await.unwrap();
}

struct Ask;
#[async_trait]
impl Tool for Ask {
    async fn call(&self, _: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("Choose environment").request_input(
            InteractionRequest::input(
                "Which environment?",
                json!({"type":"string","enum":["staging","prod"]}),
            )
            .unwrap(),
        ))
    }
}
struct AskThenEffect;
#[async_trait]
impl ChatModel for AskThenEffect {
    async fn generate(
        &self,
        _model_id: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        if request.messages.iter().any(|m| m.role == Role::Tool) {
            assert!(request.messages.iter().any(|m| {
                m.role == Role::User
                    && serde_json::to_string(&m.content)
                        .unwrap()
                        .contains("staging")
            }));
            Ok(ChatResponse::text("replan for staging"))
        } else {
            Ok(ChatResponse::tool_calls(vec![
                ToolCall::new("ask", "ask", json!({})),
                ToolCall::new("write", "double", json!({"amount":11})),
            ]))
        }
    }
}
#[tokio::test]
async fn input_pauses_before_sibling_effects_and_forces_replanning_after_answer() {
    let directory = tempfile::tempdir().unwrap();
    let store = database(&directory.path().join("input.db")).await;
    let effects = Arc::new(AtomicUsize::new(0));
    let agent = TestBuilder::default()
        .plugin(store.clone().plugin())
        .plugin(ChatModelPlugin::new(AskThenEffect))
        .plugin(AgentLoopPlugin::new(
            WorkflowLoop::new(definition()).unwrap(),
        ))
        .plugin(ToolProviderPlugin::new(
            "tools",
            StaticToolProvider::new(vec![
                ToolBinding::new(ToolDescriptor::new("ask", "ask", "Ask", json!({})), Ask),
                double_tool(effects.clone()),
            ]),
        ))
        .build()
        .await
        .unwrap();
    let handle = agent.start(RunRequest::new("input", "work")).unwrap();
    let run_id = handle.run_id;
    let waiting = handle.wait().await.unwrap();
    assert_eq!(waiting.status, RunStatus::AwaitingInput);
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    let answer = InteractionResponse {
        interaction_id: waiting.interactions[0].id,
        value: json!("staging"),
    };
    assert_eq!(
        agent
            .run(resume("input", run_id, Some(answer)))
            .await
            .unwrap()
            .output,
        "replan for staging"
    );
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    let saved = store.load(run_id).await.unwrap();
    validate_messages(&saved.transcript).unwrap();
    assert_eq!(
        saved
            .transcript
            .iter()
            .filter(|m| m.role == Role::Tool)
            .count(),
        2
    );
    agent.close().await;
    store.close().await.unwrap();
}

struct StageProvider(Arc<Mutex<Vec<(String, ChatRequest)>>>);
#[async_trait]
impl ChatModel for StageProvider {
    async fn generate(
        &self,
        model: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        let input = request
            .messages
            .iter()
            .find(|m| m.role == Role::User)
            .unwrap()
            .content
            .text();
        self.0.lock().unwrap().push((model.to_owned(), request));
        Ok(ChatResponse::text(format!("{model}: {input}")))
    }
}
#[tokio::test]
async fn stage_workflow_uses_selected_models_and_passes_prior_stage_output() {
    let directory = tempfile::tempdir().unwrap();
    let store = database(&directory.path().join("stages.db")).await;
    let calls = Arc::default();
    let definition = WorkflowDefinition {
        id: "review".into(),
        version: 1,
        stages: vec![WorkflowStage::new("plan"), WorkflowStage::new("verify")],
    };
    let strategy = StageModelStrategy {
        stages: [
            (
                "plan".into(),
                Arc::new(FixedModelStrategy(ModelRef::new("shared", "planner")))
                    as Arc<dyn ModelSelectionStrategy>,
            ),
            (
                "verify".into(),
                Arc::new(FixedModelStrategy(ModelRef::new("shared", "reviewer")))
                    as Arc<dyn ModelSelectionStrategy>,
            ),
        ]
        .into(),
        fallback: None,
    };
    let agent = TestBuilder::default()
        .plugin(store.clone().plugin())
        .plugin(AgentLoopPlugin::new(WorkflowLoop::new(definition).unwrap()))
        .plugin(ModelConnectionPlugin::new(ModelConnection::new(
            "shared",
            Arc::new(StageProvider(Arc::clone(&calls))),
            vec![
                ModelDescriptor::new("planner"),
                ModelDescriptor::new("reviewer"),
            ],
        )))
        .plugin(ModelStrategyPlugin(Arc::new(strategy)))
        .build()
        .await
        .unwrap();
    let result = agent
        .run(RunRequest::new("stages", "inspect inventory"))
        .await
        .unwrap();
    assert_eq!(result.output, "reviewer: inspect inventory");
    assert_eq!(result.steps, 2);
    let calls = calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .map(|(model, _)| model.as_str())
            .collect::<Vec<_>>(),
        ["planner", "reviewer"]
    );
    assert!(
        calls[1]
            .1
            .messages
            .iter()
            .any(|m| m.role == Role::Assistant && m.content.text() == "planner: inspect inventory")
    );
}
struct LeaseGate {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    effects: Arc<AtomicUsize>,
}
#[async_trait]
impl ToolProvider for LeaseGate {
    async fn lease(&self, _: &RunContext) -> Result<ToolLease, AgentError> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(ToolLease::new(vec![double_tool(self.effects.clone())]))
    }
}

#[tokio::test]
async fn acquired_checkpoint_is_revalidated_after_another_executor_changes_definition() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drift.db");
    let store = database(&path).await;
    let effects = Arc::new(AtomicUsize::new(0));
    let original = arithmetic_agent(
        store.clone(),
        Arc::new(AtomicUsize::new(0)),
        effects.clone(),
    )
    .await;
    let mut handle = original.start(RunRequest::new("drift", "6")).unwrap();
    let mut original_events = handle.take_events();
    let id = handle.run_id;
    let waiting = handle.wait().await.unwrap();
    original.close().await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let executor = TestBuilder::default()
        .plugin(store.clone().plugin())
        .plugin(ChatModelPlugin::new(Arithmetic {
            calls: Arc::new(AtomicUsize::new(0)),
        }))
        .plugin(AgentLoopPlugin::new(
            WorkflowLoop::new(definition()).unwrap(),
        ))
        .plugin(ToolPolicyPlugin::new("approval", Approval))
        .plugin(ToolProviderPlugin::new(
            "gate",
            LeaseGate {
                entered: entered.clone(),
                release: release.clone(),
                effects: effects.clone(),
            },
        ))
        .build()
        .await
        .unwrap();
    let mut handle = executor
        .start(resume(
            "drift",
            id,
            Some(InteractionResponse {
                interaction_id: waiting.interactions[0].id,
                value: json!(true),
            }),
        ))
        .unwrap();
    let mut resumed_events = handle.take_events();
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    let competing = database(&path).await;
    let lease = competing
        .claim(id, RunId::new_v4(), Duration::from_secs(5))
        .await
        .unwrap();
    let mut changed = competing.load(id).await.unwrap();
    let revision = changed.revision;
    let mut state: WorkflowState = serde_json::from_value(changed.state).unwrap();
    state.definition.stages[0]
        .instructions
        .push(Message::system("different workflow"));
    changed.state = serde_json::to_value(state).unwrap();
    competing.commit(&lease, revision, changed).await.unwrap();
    competing.release(&lease).await.unwrap();
    release.notify_one();
    let error = handle.wait().await.unwrap_err();
    assert!(error.to_string().contains("acquired checkpoint"), "{error}");
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    let before = original_events.try_recv().unwrap();
    let after = resumed_events.try_recv().unwrap();
    assert_eq!(before.run_id, after.run_id);
    assert_eq!(before.sequence, after.sequence);
    assert_ne!(before.scope, after.scope);
    assert!(
        competing
            .load(id)
            .await
            .unwrap()
            .events
            .iter()
            .all(|event| event.scope == EventScope::Journal)
    );
    executor.close().await;
    store.close().await.unwrap();
    competing.close().await.unwrap();
}

struct GatedDouble {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    effects: Arc<AtomicUsize>,
}
#[async_trait]
impl Tool for GatedDouble {
    async fn call(&self, args: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        self.effects.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        self.release.notified().await;
        Ok(ToolOutput::text(
            (args["amount"].as_i64().unwrap() * 2).to_string(),
        ))
    }
}
#[tokio::test]
async fn simultaneous_resume_across_connections_has_one_effect_owner() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("race.db");
    let first = database(&path).await;
    let second = database(&path).await;
    let effects = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let initial = arithmetic_agent(first.clone(), calls.clone(), effects.clone()).await;
    let handle = initial.start(RunRequest::new("race", "12")).unwrap();
    let id = handle.run_id;
    let waiting = handle.wait().await.unwrap();
    initial.close().await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let descriptor = double_tool(effects.clone()).descriptor;
    let owner = TestBuilder::default()
        .plugin(first.clone().plugin())
        .plugin(ChatModelPlugin::new(Arithmetic {
            calls: calls.clone(),
        }))
        .plugin(AgentLoopPlugin::new(
            WorkflowLoop::new(definition()).unwrap(),
        ))
        .plugin(ToolPolicyPlugin::new("approval", Approval))
        .plugin(ToolProviderPlugin::new(
            "tools",
            StaticToolProvider::new(vec![ToolBinding::new(
                descriptor,
                GatedDouble {
                    entered: entered.clone(),
                    release: release.clone(),
                    effects: effects.clone(),
                },
            )]),
        ))
        .build()
        .await
        .unwrap();
    let other = arithmetic_agent(second.clone(), calls.clone(), effects.clone()).await;
    let answer = InteractionResponse {
        interaction_id: waiting.interactions[0].id,
        value: json!(true),
    };
    let handle = owner
        .start(resume("race", id, Some(answer.clone())))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    assert!(matches!(
        other.run(resume("race", id, Some(answer.clone()))).await,
        Err(AgentError::State(StateError::Busy))
    ));
    release.notify_one();
    assert_eq!(handle.wait().await.unwrap().output, "24");
    assert_eq!(
        other
            .run(resume("race", id, Some(answer)))
            .await
            .unwrap()
            .output,
        "24"
    );
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    owner.close().await;
    other.close().await;
    first.close().await.unwrap();
    second.close().await.unwrap();
}

#[tokio::test]
async fn approval_cannot_authorize_a_changed_tool_schema() {
    let directory = tempfile::tempdir().unwrap();
    let store = database(&directory.path().join("schema.db")).await;
    let effects = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let initial = arithmetic_agent(store.clone(), calls.clone(), effects.clone()).await;
    let handle = initial.start(RunRequest::new("schema", "8")).unwrap();
    let id = handle.run_id;
    let waiting = handle.wait().await.unwrap();
    initial.close().await;
    let mut tool = double_tool(effects.clone());
    tool.descriptor.input_schema["properties"]["amount"]["minimum"] = json!(100);
    let changed = TestBuilder::default()
        .plugin(store.clone().plugin())
        .plugin(ChatModelPlugin::new(Arithmetic { calls }))
        .plugin(AgentLoopPlugin::new(
            WorkflowLoop::new(definition()).unwrap(),
        ))
        .plugin(ToolPolicyPlugin::new("approval", Approval))
        .plugin(ToolProviderPlugin::new(
            "tools",
            StaticToolProvider::new(vec![tool]),
        ))
        .build()
        .await
        .unwrap();
    assert!(
        changed
            .run(resume(
                "schema",
                id,
                Some(InteractionResponse {
                    interaction_id: waiting.interactions[0].id,
                    value: json!(true)
                })
            ))
            .await
            .is_err()
    );
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    changed.close().await;
    store.close().await.unwrap();
}

struct FailedPolicy;
#[async_trait]
impl ToolPolicy for FailedPolicy {
    async fn evaluate(
        &self,
        _: &ToolInvocationContext<'_>,
    ) -> Result<ToolPolicyDecision, AgentError> {
        Err(AgentError::Run("policy service unavailable".into()))
    }
}
struct FailedAfter;
#[async_trait]
impl ToolMiddleware for FailedAfter {
    async fn intercept(
        &self,
        _: &ToolInvocationContext<'_>,
        next: ToolNext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        next.run().await?;
        Err(ToolError::Failed("result transform failed".into()))
    }
}

#[tokio::test]
async fn policy_failure_has_no_effect_and_post_effect_interceptor_failure_requires_recovery() {
    for after in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let store = database(&directory.path().join("failure.db")).await;
        let effects = Arc::new(AtomicUsize::new(0));
        let builder = TestBuilder::default()
            .plugin(store.clone().plugin())
            .plugin(ChatModelPlugin::new(Arithmetic {
                calls: Arc::new(AtomicUsize::new(0)),
            }))
            .plugin(AgentLoopPlugin::new(
                WorkflowLoop::new(definition()).unwrap(),
            ))
            .plugin(ToolProviderPlugin::new(
                "tools",
                StaticToolProvider::new(vec![double_tool(effects.clone())]),
            ));
        let agent = if after {
            builder
                .plugin(ToolMiddlewarePlugin::new("failed-after", FailedAfter))
                .build()
                .await
                .unwrap()
        } else {
            builder
                .plugin(ToolPolicyPlugin::new("failed-policy", FailedPolicy))
                .build()
                .await
                .unwrap()
        };
        let handle = agent.start(RunRequest::new("failure", "9")).unwrap();
        let id = handle.run_id;
        let result = handle.wait().await;
        if after {
            assert_eq!(result.unwrap().status, RunStatus::RecoveryRequired);
        } else {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("policy service unavailable")
            );
        }
        assert_eq!(effects.load(Ordering::SeqCst), usize::from(after));
        assert_eq!(
            store.load(id).await.unwrap().status,
            ExecutionStatus::RecoveryRequired
        );
        let lease = store
            .claim(id, RunId::new_v4(), Duration::from_secs(1))
            .await
            .unwrap();
        store.release(&lease).await.unwrap();
        agent.close().await;
        store.close().await.unwrap();
    }
}
struct TwoEffects;
#[async_trait]
impl ChatModel for TwoEffects {
    async fn generate(
        &self,
        _model_id: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        if request.messages.iter().any(|m| m.role == Role::Tool) {
            Ok(ChatResponse::text("completed batch"))
        } else {
            Ok(ChatResponse::tool_calls(vec![
                ToolCall::new("first", "double", json!({"amount":2})),
                ToolCall::new("second", "hang", json!({})),
            ]))
        }
    }
}
#[tokio::test]
async fn finished_tool_is_reused_when_a_later_sibling_is_interrupted() {
    let directory = tempfile::tempdir().unwrap();
    let store = database(&directory.path().join("siblings.db")).await;
    let effects = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let agent = TestBuilder::default()
        .plugin(store.clone().plugin())
        .plugin(ChatModelPlugin::new(TwoEffects))
        .plugin(AgentLoopPlugin::new(
            WorkflowLoop::new(definition()).unwrap(),
        ))
        .plugin(ToolProviderPlugin::new(
            "tools",
            StaticToolProvider::new(vec![
                double_tool(effects.clone()),
                ToolBinding::new(
                    ToolDescriptor::new("hang", "hang", "hang", json!({"type":"object"})),
                    EffectThenHang {
                        effects: effects.clone(),
                        started: entered.clone(),
                    },
                ),
            ]),
        ))
        .build()
        .await
        .unwrap();
    let handle = agent.start(RunRequest::new("siblings", "work")).unwrap();
    let id = handle.run_id;
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    handle.cancel();
    assert_eq!(
        handle.wait().await.unwrap().status,
        RunStatus::RecoveryRequired
    );
    let state: WorkflowState = serde_json::from_value(store.load(id).await.unwrap().state).unwrap();
    let WorkflowCursor::Tools { executions, .. } = state.cursor else {
        panic!("tool cursor required")
    };
    assert!(matches!(
        executions[0].state,
        ToolExecutionState::Finished(_)
    ));
    assert_eq!(executions[1].state, ToolExecutionState::Started);
    let mut request = resume("siblings", id, None);
    request
        .resume
        .as_mut()
        .unwrap()
        .resolutions
        .push(CallResolution::Record {
            invocation_id: executions[1].invocation_id,
            output: ToolOutput::text("verified"),
        });
    assert_eq!(agent.run(request).await.unwrap().output, "completed batch");
    assert_eq!(effects.load(Ordering::SeqCst), 2);
    agent.close().await;
    store.close().await.unwrap();
}

struct PendingChild(Arc<tokio::sync::Notify>);
#[async_trait]
impl ChatModel for PendingChild {
    async fn generate(
        &self,
        _model_id: &str,
        _: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        self.0.notify_one();
        std::future::pending().await
    }
}
struct DelegateTool(Agent);
#[async_trait]
impl Tool for DelegateTool {
    async fn call(&self, args: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let mut request = RunRequest::new("child", args.to_string());
        request.limits.max_total_tokens = 200;
        let result = self
            .0
            .run_child(request, context)
            .await
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        Ok(ToolOutput::text(result.output))
    }
}
#[tokio::test]
async fn delegated_budget_and_count_are_durable_before_child_provider_dispatch() {
    let directory = tempfile::tempdir().unwrap();
    let store = database(&directory.path().join("child.db")).await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let child = TestBuilder::default()
        .plugin(ChatModelPlugin::new(PendingChild(entered.clone())))
        .build()
        .await
        .unwrap();
    let tool = double_tool(Arc::new(AtomicUsize::new(0))).descriptor;
    let agent = TestBuilder::default()
        .plugin(store.clone().plugin())
        .plugin(ChatModelPlugin::new(Arithmetic {
            calls: Arc::new(AtomicUsize::new(0)),
        }))
        .plugin(AgentLoopPlugin::new(
            WorkflowLoop::new(definition()).unwrap(),
        ))
        .plugin(ToolProviderPlugin::new(
            "tools",
            StaticToolProvider::new(vec![ToolBinding::new(tool, DelegateTool(child.clone()))]),
        ))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("parent", "9");
    request.limits.max_child_runs = 1;
    let handle = agent.start(request).unwrap();
    let id = handle.run_id;
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    let state: WorkflowState = serde_json::from_value(store.load(id).await.unwrap().state).unwrap();
    assert_eq!(state.child_runs, 1);
    assert!(state.budget_usage.input_tokens + state.budget_usage.output_tokens >= 200);
    handle.cancel();
    assert_eq!(
        handle.wait().await.unwrap().status,
        RunStatus::RecoveryRequired
    );
    let state: WorkflowState = serde_json::from_value(store.load(id).await.unwrap().state).unwrap();
    let WorkflowCursor::Tools { executions, .. } = state.cursor else {
        panic!("tools required")
    };
    let mut request = resume("parent", id, None);
    request
        .resume
        .as_mut()
        .unwrap()
        .resolutions
        .push(CallResolution::Retry {
            invocation_id: executions[0].invocation_id,
        });
    let result = agent.run(request).await.unwrap();
    assert!(result.output.contains("child run count limit"));
    agent.close().await;
    child.close().await;
    store.close().await.unwrap();
}

#[derive(Clone, Copy)]
enum ReleaseFault {
    Transient,
    CommittedThenFailed,
    Persistent,
    Pending,
    TerminalCommit,
    SteeringCommit,
}
struct CleanupFaultStore {
    inner: Arc<SqliteStateStore>,
    fail_acquired_load: bool,
    lease: Mutex<Option<ExecutionLease>>,
    release_fault: ReleaseFault,
    release_calls: AtomicUsize,
}
#[async_trait]
impl StateStore for CleanupFaultStore {
    async fn create(
        &self,
        checkpoint: Checkpoint,
        previous: Option<RunId>,
    ) -> Result<Checkpoint, StateError> {
        self.inner.create(checkpoint, previous).await
    }
    async fn load(&self, id: RunId) -> Result<Checkpoint, StateError> {
        if self.fail_acquired_load && self.lease.lock().unwrap().is_some() {
            return Err(StateError::Backend(
                "acquired checkpoint read failed".into(),
            ));
        }
        self.inner.load(id).await
    }
    async fn latest(&self, session: &str) -> Result<Option<Checkpoint>, StateError> {
        self.inner.latest(session).await
    }
    async fn claim(
        &self,
        id: RunId,
        owner: RunId,
        ttl: Duration,
    ) -> Result<ExecutionLease, StateError> {
        let lease = self.inner.claim(id, owner, ttl).await?;
        *self.lease.lock().unwrap() = Some(lease.clone());
        Ok(lease)
    }
    async fn renew(
        &self,
        lease: &ExecutionLease,
        ttl: Duration,
    ) -> Result<ExecutionLease, StateError> {
        self.inner.renew(lease, ttl).await
    }
    async fn commit(
        &self,
        lease: &ExecutionLease,
        revision: u64,
        checkpoint: Checkpoint,
    ) -> Result<Checkpoint, StateError> {
        if matches!(self.release_fault, ReleaseFault::SteeringCommit)
            && checkpoint
                .events
                .iter()
                .any(|e| matches!(e.event, AgentEvent::Steered(_)))
        {
            return Err(StateError::Backend(
                "steering checkpoint write failed".into(),
            ));
        }
        if matches!(self.release_fault, ReleaseFault::TerminalCommit)
            && checkpoint.status.is_terminal()
        {
            return Err(StateError::Backend(
                "terminal checkpoint write failed".into(),
            ));
        }
        self.inner.commit(lease, revision, checkpoint).await
    }
    async fn release(&self, lease: &ExecutionLease) -> Result<(), StateError> {
        let attempt = self.release_calls.fetch_add(1, Ordering::SeqCst);
        match self.release_fault {
            ReleaseFault::Pending => std::future::pending().await,
            ReleaseFault::Persistent => {
                Err(StateError::Backend("lease release unavailable".into()))
            }
            ReleaseFault::Transient if attempt == 0 => {
                Err(StateError::Backend("transient release error".into()))
            }
            ReleaseFault::CommittedThenFailed if attempt == 0 => {
                self.inner.release(lease).await?;
                Err(StateError::Backend("release response lost".into()))
            }
            _ => self.inner.release(lease).await,
        }
    }
}
struct CleanupModel;
#[async_trait]
impl ChatModel for CleanupModel {
    async fn generate(
        &self,
        _model_id: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        if request
            .messages
            .last()
            .is_some_and(|m| m.content.text() == "fail generation")
        {
            return Err(ModelError::Fatal("generation failed".into()));
        }
        Ok(ChatResponse::text("completed"))
    }
}
async fn cleanup_agent(store: Arc<CleanupFaultStore>) -> Agent {
    TestBuilder::default()
        .plugin(StateStorePlugin(store))
        .plugin(ChatModelPlugin::new(CleanupModel))
        .plugin(AgentLoopPlugin::new(
            WorkflowLoop::new(definition()).unwrap(),
        ))
        .build()
        .await
        .unwrap()
}

#[tokio::test]
async fn workflow_preserves_primary_and_release_failures_with_bounded_cleanup() {
    for fault in [ReleaseFault::Persistent, ReleaseFault::Pending] {
        let directory = tempfile::tempdir().unwrap();
        let database = database(&directory.path().join("release-failure.db")).await;
        let store = Arc::new(CleanupFaultStore {
            inner: database.clone(),
            fail_acquired_load: true,
            lease: Mutex::new(None),
            release_fault: fault,
            release_calls: AtomicUsize::new(0),
        });
        let agent = cleanup_agent(store.clone()).await;
        let mut request = RunRequest::new("release", "input");
        request.limits.cleanup_timeout = Duration::from_millis(100);
        let error = tokio::time::timeout(Duration::from_secs(2), agent.run(request))
            .await
            .unwrap()
            .unwrap_err();
        let AgentError::WithCleanup { primary, cleanup } = &error else {
            panic!("both typed errors required: {error}");
        };
        assert!(
            matches!(primary.as_ref(), AgentError::State(StateError::Backend(message)) if message == "acquired checkpoint read failed")
        );
        let expected = if matches!(fault, ReleaseFault::Pending) {
            "timed out"
        } else {
            "lease release unavailable"
        };
        assert!(
            matches!(cleanup.as_ref(), AgentError::State(StateError::Backend(message)) if message.contains(expected))
        );
        assert!(matches!(error.primary(), AgentError::State(_)));
        tokio::task::yield_now().await;
        assert_eq!(
            store.release_calls.load(Ordering::SeqCst),
            if matches!(fault, ReleaseFault::Pending) {
                1
            } else {
                2
            }
        );
        let lease = store.lease.lock().unwrap().clone().unwrap();
        assert!(matches!(
            database
                .claim(lease.run_id, RunId::new_v4(), Duration::from_secs(30))
                .await,
            Err(StateError::Busy)
        ));
        database.release(&lease).await.unwrap();
        agent.close().await;
        database.close().await.unwrap();
    }
}

#[tokio::test]
async fn transient_or_uncertain_release_is_retried_without_masking_execution_error() {
    for fault in [ReleaseFault::Transient, ReleaseFault::CommittedThenFailed] {
        let directory = tempfile::tempdir().unwrap();
        let database = database(&directory.path().join("release-retry.db")).await;
        let store = Arc::new(CleanupFaultStore {
            inner: database.clone(),
            fail_acquired_load: true,
            lease: Mutex::new(None),
            release_fault: fault,
            release_calls: AtomicUsize::new(0),
        });
        let agent = cleanup_agent(store.clone()).await;
        let error = agent
            .run(RunRequest::new("release", "input"))
            .await
            .unwrap_err();
        assert!(
            matches!(error, AgentError::State(StateError::Backend(ref message)) if message == "acquired checkpoint read failed")
        );
        assert_eq!(store.release_calls.load(Ordering::SeqCst), 2);
        let previous = store.lease.lock().unwrap().clone().unwrap();
        let replacement = database
            .claim(previous.run_id, RunId::new_v4(), Duration::from_secs(30))
            .await
            .unwrap();
        assert!(replacement.generation > previous.generation);
        database.release(&replacement).await.unwrap();
        agent.close().await;
        database.close().await.unwrap();
    }
}

#[tokio::test]
async fn release_failure_after_success_is_reported_without_rewriting_terminal_checkpoint() {
    let directory = tempfile::tempdir().unwrap();
    let database = database(&directory.path().join("completed-release.db")).await;
    let store = Arc::new(CleanupFaultStore {
        inner: database.clone(),
        fail_acquired_load: false,
        lease: Mutex::new(None),
        release_fault: ReleaseFault::Persistent,
        release_calls: AtomicUsize::new(0),
    });
    let agent = cleanup_agent(store.clone()).await;
    let handle = agent.start(RunRequest::new("release", "input")).unwrap();
    let id = handle.run_id;
    assert!(
        matches!(handle.wait().await, Err(AgentError::State(StateError::Backend(message))) if message == "lease release unavailable")
    );
    assert_eq!(
        database.load(id).await.unwrap().status,
        ExecutionStatus::Completed
    );
    assert_eq!(store.release_calls.load(Ordering::SeqCst), 2);
    agent.close().await;
    database.close().await.unwrap();
}

#[tokio::test]
async fn terminal_write_failure_retains_the_execution_error_and_leaves_durable_state_unchanged() {
    let directory = tempfile::tempdir().unwrap();
    let database = database(&directory.path().join("terminal-write.db")).await;
    let store = Arc::new(CleanupFaultStore {
        inner: database.clone(),
        fail_acquired_load: false,
        lease: Mutex::new(None),
        release_fault: ReleaseFault::TerminalCommit,
        release_calls: AtomicUsize::new(0),
    });
    let agent = cleanup_agent(store.clone()).await;
    let handle = agent
        .start(RunRequest::new("terminal-write", "fail generation"))
        .unwrap();
    let id = handle.run_id;
    let error = handle.wait().await.unwrap_err();
    let AgentError::WithCleanup { primary, cleanup } = error else {
        panic!("execution and finalization failures required");
    };
    assert!(
        matches!(*primary, AgentError::Model(ModelError::Fatal(message)) if message == "generation failed")
    );
    assert!(
        matches!(*cleanup, AgentError::State(StateError::Backend(message)) if message == "terminal checkpoint write failed")
    );
    assert_eq!(
        database.load(id).await.unwrap().status,
        ExecutionStatus::Running
    );
    assert_eq!(store.release_calls.load(Ordering::SeqCst), 1);
    let next = database
        .claim(id, RunId::new_v4(), Duration::from_secs(30))
        .await
        .unwrap();
    database.release(&next).await.unwrap();
    agent.close().await;
    database.close().await.unwrap();
}
