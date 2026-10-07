use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_storage_local::*;
use eliteforge_agent_test_support::*;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Barrier, Notify};

fn definition(
    nodes: Vec<DagNode>,
    edges: &[(&str, &str)],
    parallel: usize,
) -> DagWorkflowDefinition {
    DagWorkflowDefinition {
        id: "test-dag".into(),
        version: 1,
        nodes,
        edges: edges.iter().map(|(a, b)| DagEdge::new(*a, *b)).collect(),
        max_parallel_nodes: parallel,
    }
}
fn node(id: &str, config: Value) -> DagNode {
    DagNode::new(id, "test", config)
}
async fn store(path: &std::path::Path) -> Arc<SqliteStateStore> {
    Arc::new(
        SqliteStateStore::open(path, SqliteOptions::default())
            .await
            .unwrap(),
    )
}
fn resume(session: &str, run_id: RunId) -> RunRequest {
    let mut r = RunRequest::new(session, "ignored during resume");
    r.resume = Some(ResumeRequest {
        run_id,
        ..Default::default()
    });
    r
}
async fn state(store: &dyn StateStore, id: RunId) -> DagWorkflowState {
    serde_json::from_value(store.load(id).await.unwrap().state).unwrap()
}
#[derive(Default, Clone)]
struct TestNode {
    calls: Arc<Mutex<Vec<String>>>,
    barrier: Option<Arc<Barrier>>,
    entered: Arc<Notify>,
    dropped: Arc<AtomicUsize>,
}
struct DropCount(Arc<AtomicUsize>);
impl Drop for DropCount {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
#[async_trait]
impl WorkflowNode for TestNode {
    fn supports_tool_policy(&self) -> bool {
        true
    }
    fn kind(&self) -> &str {
        "test"
    }
    async fn execute(
        &self,
        input: WorkflowNodeInput,
        context: &WorkflowNodeContext,
        _: &AgentServices,
    ) -> Result<WorkflowNodeResult, AgentError> {
        self.calls.lock().unwrap().push(input.node_id.clone());
        let _drop = DropCount(self.dropped.clone());
        if input.config["barrier"] == true {
            self.barrier.as_ref().unwrap().wait().await;
        }
        if input.config["save"] == true {
            context.save(json!({"saved": input.node_id})).await?;
        }
        if input.config["wait"] == true {
            self.entered.notify_one();
            std::future::pending::<()>().await;
        }
        if input.config["fail"] == true {
            if self.barrier.is_none() {
                self.entered.notified().await;
            }
            return Err(AgentError::Run("fixture failed".into()));
        }
        let value = if input.config["sum"] == true {
            json!(
                input
                    .predecessors
                    .values()
                    .map(|v| v.as_i64().unwrap())
                    .sum::<i64>()
            )
        } else {
            input.config.get("value").cloned().unwrap_or(json!(7))
        };
        let mut result = WorkflowNodeResult::completed(value);
        if let Some(routes) = input.config.get("routes") {
            result.routes = Some(serde_json::from_value(routes.clone()).unwrap());
        }
        Ok(result)
    }
}
async fn agent(
    store: Arc<dyn StateStore>,
    definition: DagWorkflowDefinition,
    implementation: TestNode,
) -> Agent {
    TestBuilder::default()
        .plugin(StateStorePlugin(store))
        .plugin(WorkflowNodePlugin::new("test-nodes", implementation))
        .plugin(AgentLoopPlugin::new(
            DagWorkflowLoop::new(definition).unwrap(),
        ))
        .build()
        .await
        .unwrap()
}

#[test]
fn definitions_validate_cycles_edges_ids_and_serial_compilation() {
    let good = definition(
        vec![node("a", json!({})), node("b", json!({}))],
        &[("a", "b")],
        2,
    );
    good.validate().unwrap();
    let mut bad = good.clone();
    bad.edges.push(DagEdge::new("b", "a"));
    assert!(bad.validate().is_err());
    let mut bad = good.clone();
    bad.edges.push(DagEdge::new("a", "missing"));
    assert!(bad.validate().is_err());
    let mut bad = good.clone();
    bad.edges.push(DagEdge::new("a", "b"));
    assert!(bad.validate().is_err());
    let mut bad = good.clone();
    bad.nodes[1].id = "a".into();
    assert!(bad.validate().is_err());
    let mut bad = good;
    bad.max_parallel_nodes = 0;
    assert!(bad.validate().is_err());
    let chain = DagWorkflowDefinition::from_stages(WorkflowDefinition {
        id: "chain".into(),
        version: 2,
        stages: vec![WorkflowStage::new("plan"), WorkflowStage::new("review")],
    })
    .unwrap();
    assert_eq!(chain.edges, vec![DagEdge::new("plan", "review")]);
    chain.validate().unwrap();
}

#[tokio::test]
async fn parallel_nodes_overlap_join_json_and_replay_after_reopening_database() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("dag.db");
    let db = store(&path).await;
    let implementation = TestNode {
        barrier: Some(Arc::new(Barrier::new(2))),
        ..Default::default()
    };
    let graph = definition(
        vec![
            node("a", json!({"barrier":true,"save":true})),
            node("b", json!({"barrier":true,"save":true})),
            node("join", json!({"sum":true})),
        ],
        &[("a", "join"), ("b", "join")],
        2,
    );
    let a = agent(db.clone(), graph.clone(), implementation.clone()).await;
    let h = a.start(RunRequest::new("parallel", "input")).unwrap();
    let id = h.run_id;
    let result = tokio::time::timeout(Duration::from_secs(3), a.wait(h))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.structured, Some(json!(14)));
    let saved = state(db.as_ref(), id).await;
    assert_eq!(saved.nodes["a"].checkpoint, Some(json!({"saved":"a"})));
    assert_eq!(saved.nodes["b"].checkpoint, Some(json!({"saved":"b"})));
    let events = db.load(id).await.unwrap().events;
    assert_eq!(events.len(), 6);
    assert!(
        events
            .windows(2)
            .all(|p| p[0].sequence + 1 == p[1].sequence)
    );
    a.framework().close().await;
    db.close().await.unwrap();
    let reopened = store(&path).await;
    let fresh = TestNode::default();
    let replay = agent(reopened.clone(), graph, fresh.clone()).await;
    assert_eq!(
        replay.run(resume("parallel", id)).await.unwrap().structured,
        Some(json!(14))
    );
    assert!(fresh.calls.lock().unwrap().is_empty());
    replay.framework().close().await;
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn selected_branch_skips_transitively_and_join_does_not_wait_for_pruned_inputs() {
    for join in [WorkflowJoin::Selected, WorkflowJoin::All] {
        let temp = tempfile::tempdir().unwrap();
        let db = store(&temp.path().join("branch.db")).await;
        let mut join_node = node("join", json!({"sum":true}));
        join_node.join = join;
        let graph = definition(
            vec![
                node("route", json!({"routes":["left"]})),
                node("left", json!({})),
                node("right", json!({})),
                node("right_child", json!({})),
                join_node,
            ],
            &[
                ("route", "left"),
                ("route", "right"),
                ("right", "right_child"),
                ("left", "join"),
                ("right_child", "join"),
            ],
            3,
        );
        let implementation = TestNode::default();
        let a = agent(db.clone(), graph, implementation.clone()).await;
        let h = a.start(RunRequest::new("branch", "input")).unwrap();
        let id = h.run_id;
        tokio::time::timeout(Duration::from_secs(3), a.wait(h))
            .await
            .unwrap()
            .unwrap();
        let s = state(db.as_ref(), id).await;
        assert_eq!(s.nodes["right"].status, WorkflowNodeStatus::Skipped);
        assert_eq!(s.nodes["right_child"].status, WorkflowNodeStatus::Skipped);
        assert_eq!(
            s.nodes["join"].status,
            if join == WorkflowJoin::Selected {
                WorkflowNodeStatus::Succeeded
            } else {
                WorkflowNodeStatus::Skipped
            }
        );
        assert!(
            !implementation
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|id| id.starts_with("right"))
        );
        a.framework().close().await;
        db.close().await.unwrap();
    }
}

#[tokio::test]
async fn cancellation_drops_owned_tasks_and_is_terminal() {
    let temp = tempfile::tempdir().unwrap();
    let db = store(&temp.path().join("cancel.db")).await;
    let implementation = TestNode::default();
    let graph = definition(
        vec![
            node("effect", json!({"wait":true})),
            node("next", json!({"sum":true})),
        ],
        &[("effect", "next")],
        2,
    );
    let a = agent(db.clone(), graph.clone(), implementation.clone()).await;
    let h = a.start(RunRequest::new("cancel", "input")).unwrap();
    let id = h.run_id;
    implementation.entered.notified().await;
    a.cancel(&h).unwrap();
    assert!(matches!(a.wait(h).await, Err(AgentError::Cancelled)));
    assert_eq!(implementation.dropped.load(Ordering::SeqCst), 1);
    assert_eq!(
        db.load(id).await.unwrap().status,
        ExecutionStatus::Cancelled
    );
    assert!(matches!(
        a.run(resume("cancel", id)).await,
        Err(AgentError::Cancelled)
    ));
    assert_eq!(implementation.calls.lock().unwrap().as_slice(), &["effect"]);
    a.framework().close().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn failing_node_cancels_pending_sibling_and_does_not_start_downstream() {
    let temp = tempfile::tempdir().unwrap();
    let db = store(&temp.path().join("fail.db")).await;
    let implementation = TestNode::default();
    let graph = definition(
        vec![
            node("wait", json!({"wait":true})),
            node("fail", json!({"fail":true})),
            node("next", json!({})),
        ],
        &[("fail", "next")],
        2,
    );
    let a = agent(db.clone(), graph, implementation.clone()).await;
    let h = a.start(RunRequest::new("fail", "input")).unwrap();
    let id = h.run_id;
    let error = tokio::time::timeout(Duration::from_secs(3), a.wait(h))
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("fixture failed"));
    assert_eq!(implementation.dropped.load(Ordering::SeqCst), 2);
    let s = state(db.as_ref(), id).await;
    assert_eq!(s.nodes["next"].status, WorkflowNodeStatus::Pending);
    assert_eq!(
        s.nodes["fail"].error.as_deref(),
        Some("run failed: fixture failed")
    );
    a.framework().close().await;
    db.close().await.unwrap();
}

#[derive(Clone, Copy)]
enum Fault {
    Intent,
    Result,
    CommittedResult,
}
struct FaultStore {
    inner: Arc<SqliteStateStore>,
    fault: Fault,
}
#[async_trait]
impl StateStore for FaultStore {
    async fn create(&self, c: Checkpoint, p: Option<RunId>) -> Result<Checkpoint, StateError> {
        self.inner.create(c, p).await
    }
    async fn load(&self, id: RunId) -> Result<Checkpoint, StateError> {
        self.inner.load(id).await
    }
    async fn latest(&self, s: &str) -> Result<Option<Checkpoint>, StateError> {
        self.inner.latest(s).await
    }
    async fn latest_scope(
        &self,
        session: &str,
        scope: &str,
    ) -> Result<Option<Checkpoint>, StateError> {
        self.inner.latest_scope(session, scope).await
    }
    async fn claim(
        &self,
        id: RunId,
        owner: RunId,
        ttl: Duration,
    ) -> Result<ExecutionLease, StateError> {
        self.inner.claim(id, owner, ttl).await
    }
    async fn renew(&self, l: &ExecutionLease, ttl: Duration) -> Result<ExecutionLease, StateError> {
        self.inner.renew(l, ttl).await
    }
    async fn release(&self, l: &ExecutionLease) -> Result<(), StateError> {
        self.inner.release(l).await
    }
    async fn commit(
        &self,
        l: &ExecutionLease,
        revision: u64,
        c: Checkpoint,
    ) -> Result<Checkpoint, StateError> {
        let state: DagWorkflowState = serde_json::from_value(c.state.clone()).unwrap();
        let status = state.nodes["a"].status;
        let fail = match self.fault {
            Fault::Intent => status == WorkflowNodeStatus::Running,
            _ => status == WorkflowNodeStatus::Succeeded,
        };
        if fail {
            if matches!(self.fault, Fault::CommittedResult) {
                self.inner.commit(l, revision, c).await?;
            }
            return Err(StateError::Backend("injected storage failure".into()));
        }
        self.inner.commit(l, revision, c).await
    }
}

#[tokio::test]
async fn persistence_errors_stop_dispatch_and_uncertain_result_is_not_overwritten() {
    for fault in [Fault::Intent, Fault::Result, Fault::CommittedResult] {
        let temp = tempfile::tempdir().unwrap();
        let db = store(&temp.path().join("fault.db")).await;
        let implementation = TestNode::default();
        let graph = definition(
            vec![
                node("a", json!({"value":{"nested":[1,2,3]}})),
                node("b", json!({})),
            ],
            &[("a", "b")],
            1,
        );
        let a = agent(
            Arc::new(FaultStore {
                inner: db.clone(),
                fault,
            }),
            graph.clone(),
            implementation.clone(),
        )
        .await;
        let h = a.start(RunRequest::new("fault", "input")).unwrap();
        let id = h.run_id;
        assert!(matches!(
            a.wait(h).await,
            Err(AgentError::State(StateError::Backend(_)))
        ));
        assert_eq!(
            implementation.calls.lock().unwrap().len(),
            if matches!(fault, Fault::Intent) { 0 } else { 1 }
        );
        let s = state(db.as_ref(), id).await;
        assert_eq!(s.nodes["b"].status, WorkflowNodeStatus::Pending);
        let expected = match fault {
            Fault::Intent => WorkflowNodeStatus::Pending,
            Fault::Result => WorkflowNodeStatus::Running,
            Fault::CommittedResult => WorkflowNodeStatus::Succeeded,
        };
        assert_eq!(s.nodes["a"].status, expected);
        a.framework().close().await;
        if matches!(fault, Fault::CommittedResult) {
            let recovered = agent(db.clone(), graph, implementation.clone()).await;
            recovered.run(resume("fault", id)).await.unwrap();
            assert_eq!(
                implementation
                    .calls
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|id| *id == "a")
                    .count(),
                1
            );
            assert_eq!(
                state(db.as_ref(), id).await.nodes["a"]
                    .result
                    .as_ref()
                    .unwrap()
                    .value,
                json!({"nested":[1,2,3]})
            );
            recovered.framework().close().await;
        }
        db.close().await.unwrap();
    }
}

#[tokio::test]
async fn plugin_kinds_and_configuration_are_validated_before_run() {
    let temp = tempfile::tempdir().unwrap();
    let db = store(&temp.path().join("plugin.db")).await;
    let graph = definition(
        vec![DagNode::new("missing", "uninstalled", Value::Null)],
        &[],
        1,
    );
    let result = TestBuilder::default()
        .plugin(StateStorePlugin(db.clone()))
        .plugin(AgentLoopPlugin::new(DagWorkflowLoop::new(graph).unwrap()))
        .build()
        .await;
    assert!(matches!(result, Err(AgentError::InvalidConfiguration(_))));
    assert!(
        WorkflowNodeRegistry::new(vec![
            Arc::new(TestNode::default()),
            Arc::new(TestNode::default())
        ])
        .is_err()
    );
    assert!(WorkflowNodeRegistry::new(vec![Arc::new(ReactWorkflowNode)]).is_ok());
    assert!(
        WorkflowNodeRegistry::new(vec![
            Arc::new(ReactWorkflowNode),
            Arc::new(ReactWorkflowNode)
        ])
        .is_err()
    );
    db.close().await.unwrap();
}

struct ParallelModel {
    barrier: Arc<Barrier>,
    calls: Arc<AtomicUsize>,
}
#[async_trait]
impl ChatModel for ParallelModel {
    async fn generate(
        &self,
        _model_id: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.barrier.wait().await;
        let mut response = ChatResponse::text(request.messages.last().unwrap().content.text());
        response.usage = Usage {
            input_tokens: 3,
            output_tokens: 4,
        };
        Ok(response)
    }
}

#[tokio::test]
async fn parallel_react_nodes_share_budget_without_losing_nested_checkpoints() {
    let temp = tempfile::tempdir().unwrap();
    let db = store(&temp.path().join("react.db")).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let graph = definition(
        vec![
            DagNode::react(WorkflowStage::new("a")),
            DagNode::react(WorkflowStage::new("b")),
        ],
        &[],
        2,
    );
    let a = TestBuilder::default()
        .plugin(StateStorePlugin(db.clone()))
        .plugin(ChatModelPlugin::new(ParallelModel {
            barrier: Arc::new(Barrier::new(2)),
            calls: calls.clone(),
        }))
        .plugin(AgentLoopPlugin::new(DagWorkflowLoop::new(graph).unwrap()))
        .build()
        .await
        .unwrap();
    let h = a.start(RunRequest::new("react", "input")).unwrap();
    let id = h.run_id;
    let result = tokio::time::timeout(Duration::from_secs(3), a.wait(h))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(result.usage.input_tokens, 6);
    assert_eq!(result.usage.output_tokens, 8);
    let s = state(db.as_ref(), id).await;
    assert_eq!(s.budget_usage, result.usage);
    for n in s.nodes.values() {
        let cp: Checkpoint = serde_json::from_value(n.checkpoint.clone().unwrap()).unwrap();
        assert_eq!(cp.status, ExecutionStatus::Completed);
        cp.validate().unwrap();
    }
    a.framework().close().await;
    db.close().await.unwrap();
}

struct ToolModel;
#[async_trait]
impl ChatModel for ToolModel {
    async fn generate(
        &self,
        _model_id: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        let mut result = if request.messages.iter().any(|m| m.role == Role::Tool) {
            ChatResponse::text("tool completed")
        } else {
            ChatResponse::tool_calls(vec![ToolCall::new("effect-call", "effect", json!({}))])
        };
        result.usage = Usage {
            input_tokens: 1,
            output_tokens: 1,
        };
        Ok(result)
    }
}
struct Effect {
    calls: Arc<AtomicUsize>,
    entered: Arc<Notify>,
    wait: bool,
}
#[async_trait]
impl Tool for Effect {
    async fn call(&self, _: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        if self.wait {
            std::future::pending::<()>().await;
        }
        Ok(ToolOutput::text("effect recorded"))
    }
}
struct Approval;
#[async_trait]
impl ToolPolicy for Approval {
    async fn evaluate(
        &self,
        _: &ToolInvocationContext<'_>,
    ) -> Result<ToolPolicyDecision, AgentError> {
        Ok(ToolPolicyDecision::RequireApproval("Approve?".into()))
    }
}
async fn tool_agent(db: Arc<SqliteStateStore>, effect: Effect, approval: bool) -> Agent {
    let graph = definition(
        vec![
            DagNode::react(WorkflowStage::new("react")),
            node("after", json!({"value":"finished"})),
        ],
        &[("react", "after")],
        2,
    );
    let mut builder = TestBuilder::default()
        .plugin(StateStorePlugin(db))
        .plugin(ChatModelPlugin::new(ToolModel))
        .plugin(WorkflowNodePlugin::new("test", TestNode::default()))
        .plugin(AgentLoopPlugin::new(DagWorkflowLoop::new(graph).unwrap()))
        .plugin(ToolProviderPlugin::new(
            "tools",
            StaticToolProvider::new(vec![ToolBinding::new(
                ToolDescriptor::new("effect", "effect", "effect", json!({})),
                effect,
            )]),
        ));
    if approval {
        builder = builder.plugin(ToolPolicyPlugin::new("approve", Approval));
    }
    builder.build().await.unwrap()
}

#[tokio::test]
async fn embedded_react_approval_resumes_only_after_matching_answer() {
    let temp = tempfile::tempdir().unwrap();
    let db = store(&temp.path().join("approval.db")).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let a = tool_agent(
        db.clone(),
        Effect {
            calls: calls.clone(),
            entered: Arc::new(Notify::new()),
            wait: false,
        },
        true,
    )
    .await;
    let h = a.start(RunRequest::new("approval", "input")).unwrap();
    let id = h.run_id;
    let waiting = a.wait(h).await.unwrap();
    assert_eq!(waiting.status, RunStatus::AwaitingInput);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let mut r = resume("approval", id);
    r.resume.as_mut().unwrap().response = Some(InteractionResponse {
        interaction_id: waiting.interactions[0].id,
        value: json!(true),
    });
    assert_eq!(a.run(r).await.unwrap().output, "finished");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    a.framework().close().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn cancelled_embedded_tool_is_not_repeated() {
    let temp = tempfile::tempdir().unwrap();
    let db = store(&temp.path().join("unknown.db")).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(Notify::new());
    let a = tool_agent(
        db.clone(),
        Effect {
            calls: calls.clone(),
            entered: entered.clone(),
            wait: true,
        },
        false,
    )
    .await;
    let h = a.start(RunRequest::new("unknown", "input")).unwrap();
    let id = h.run_id;
    entered.notified().await;
    a.cancel(&h).unwrap();
    assert!(matches!(a.wait(h).await, Err(AgentError::Cancelled)));
    assert_eq!(
        db.load(id).await.unwrap().status,
        ExecutionStatus::Cancelled
    );
    assert!(matches!(
        a.run(resume("unknown", id)).await,
        Err(AgentError::Cancelled)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    a.framework().close().await;
    db.close().await.unwrap();
}

struct LimitedNode {
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}
struct ActiveCount(Arc<AtomicUsize>);
impl Drop for ActiveCount {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
#[async_trait]
impl WorkflowNode for LimitedNode {
    fn kind(&self) -> &str {
        "limited"
    }
    async fn execute(
        &self,
        _: WorkflowNodeInput,
        _: &WorkflowNodeContext,
        _: &AgentServices,
    ) -> Result<WorkflowNodeResult, AgentError> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        let _guard = ActiveCount(self.active.clone());
        self.peak.fetch_max(active, Ordering::SeqCst);
        // Keep work active across several renewal intervals. A 90ms lease is
        // too close to scheduler jitter when the workspace test suite runs in
        // parallel, which can turn this fencing test into a timing test.
        tokio::time::sleep(Duration::from_millis(600)).await;
        Ok(WorkflowNodeResult::completed(json!(active)))
    }
}
#[tokio::test]
async fn concurrency_limit_and_repeated_lease_renewals_preserve_fencing() {
    let temp = tempfile::tempdir().unwrap();
    let db = store(&temp.path().join("limit.db")).await;
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let graph = definition(
        (0..6)
            .map(|i| DagNode::new(i.to_string(), "limited", Value::Null))
            .collect(),
        &[],
        2,
    );
    let a = TestBuilder::default()
        .plugin(StateStorePlugin(db.clone()))
        .plugin(WorkflowNodePlugin::new(
            "limited",
            LimitedNode {
                active: active.clone(),
                peak: peak.clone(),
            },
        ))
        .plugin(AgentLoopPlugin::new(
            DagWorkflowLoop::new(graph)
                .unwrap()
                .with_lease_duration(Duration::from_millis(400))
                .unwrap(),
        ))
        .build()
        .await
        .unwrap();
    let h = a.start(RunRequest::new("limit", "input")).unwrap();
    let id = h.run_id;
    assert_eq!(a.wait(h).await.unwrap().status, RunStatus::Completed);
    assert_eq!(peak.load(Ordering::SeqCst), 2);
    assert_eq!(active.load(Ordering::SeqCst), 0);
    assert_eq!(
        db.load(id).await.unwrap().status,
        ExecutionStatus::Completed
    );
    a.framework().close().await;
    db.close().await.unwrap();
}

struct FlakyNode(Arc<AtomicUsize>);
#[async_trait]
impl WorkflowNode for FlakyNode {
    fn kind(&self) -> &str {
        "flaky"
    }
    async fn execute(
        &self,
        _: WorkflowNodeInput,
        _: &WorkflowNodeContext,
        _: &AgentServices,
    ) -> Result<WorkflowNodeResult, AgentError> {
        if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(AgentError::Run("first attempt failed".into()));
        }
        Ok(WorkflowNodeResult::completed(json!("recovered")))
    }
}
#[tokio::test]
async fn failed_node_is_not_retried_by_resume_but_a_new_run_can_start() {
    let temp = tempfile::tempdir().unwrap();
    let db = store(&temp.path().join("retry.db")).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let graph = definition(vec![DagNode::new("flaky", "flaky", Value::Null)], &[], 1);
    let a = TestBuilder::default()
        .plugin(StateStorePlugin(db.clone()))
        .plugin(WorkflowNodePlugin::new("flaky", FlakyNode(calls.clone())))
        .plugin(AgentLoopPlugin::new(DagWorkflowLoop::new(graph).unwrap()))
        .build()
        .await
        .unwrap();
    let h = a.start(RunRequest::new("retry", "input")).unwrap();
    let id = h.run_id;
    assert!(a.wait(h).await.is_err());
    assert_eq!(db.load(id).await.unwrap().status, ExecutionStatus::Failed);
    assert!(matches!(
        a.run(resume("retry", id)).await,
        Err(AgentError::Run(_))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let next = a.start(RunRequest::new("retry", "try again")).unwrap();
    assert_ne!(next.run_id, id);
    assert_eq!(a.wait(next).await.unwrap().output, "recovered");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    a.framework().close().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn parallel_approvals_can_be_answered_individually_without_replaying_siblings() {
    let temp = tempfile::tempdir().unwrap();
    let db = store(&temp.path().join("approvals.db")).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let graph = definition(
        vec![
            DagNode::react(WorkflowStage::new("a")),
            DagNode::react(WorkflowStage::new("b")),
        ],
        &[],
        2,
    );
    let a = TestBuilder::default()
        .plugin(StateStorePlugin(db.clone()))
        .plugin(ChatModelPlugin::new(ToolModel))
        .plugin(AgentLoopPlugin::new(DagWorkflowLoop::new(graph).unwrap()))
        .plugin(ToolPolicyPlugin::new("approve", Approval))
        .plugin(ToolProviderPlugin::new(
            "tools",
            StaticToolProvider::new(vec![ToolBinding::new(
                ToolDescriptor::new("effect", "effect", "effect", json!({})),
                Effect {
                    calls: calls.clone(),
                    entered: Arc::new(Notify::new()),
                    wait: false,
                },
            )]),
        ))
        .build()
        .await
        .unwrap();
    let h = a.start(RunRequest::new("approvals", "input")).unwrap();
    let id = h.run_id;
    let waiting = a.wait(h).await.unwrap();
    assert_eq!(waiting.interactions.len(), 2);
    let mut first = resume("approvals", id);
    first.resume.as_mut().unwrap().response = Some(InteractionResponse {
        interaction_id: waiting.interactions[0].id,
        value: json!(true),
    });
    let waiting = a.run(first).await.unwrap();
    assert_eq!(waiting.status, RunStatus::AwaitingInput);
    assert_eq!(waiting.interactions.len(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let mut second = resume("approvals", id);
    second.resume.as_mut().unwrap().response = Some(InteractionResponse {
        interaction_id: waiting.interactions[0].id,
        value: json!(true),
    });
    assert_eq!(
        a.run(second.clone()).await.unwrap().status,
        RunStatus::Completed
    );
    assert_eq!(a.run(second).await.unwrap().status, RunStatus::Completed);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    a.framework().close().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn invalid_route_does_not_dispatch_a_successor() {
    let temp = tempfile::tempdir().unwrap();
    let db = store(&temp.path().join("route.db")).await;
    let implementation = TestNode::default();
    let graph = definition(
        vec![
            node("a", json!({"routes":["absent"]})),
            node("b", json!({})),
        ],
        &[("a", "b")],
        1,
    );
    let a = agent(db.clone(), graph, implementation.clone()).await;
    assert!(matches!(
        a.run(RunRequest::new("route", "input")).await,
        Err(AgentError::InvalidConfiguration(_))
    ));
    assert_eq!(implementation.calls.lock().unwrap().as_slice(), &["a"]);
    a.framework().close().await;
    db.close().await.unwrap();
}

struct PeakTool {
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}
#[async_trait]
impl Tool for PeakTool {
    async fn call(&self, _: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        let _guard = ActiveCount(self.active.clone());
        self.peak.fetch_max(active, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(30)).await;
        Ok(ToolOutput::text("done"))
    }
}
#[tokio::test]
async fn tool_parallel_safety_and_run_wide_slot_limit_apply_across_dag_nodes() {
    for (parallel_safe, slots, expected_peak) in [(false, 2, 1), (true, 2, 2), (true, 1, 1)] {
        let temp = tempfile::tempdir().unwrap();
        let db = store(&temp.path().join("tools.db")).await;
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut binding = ToolBinding::new(
            ToolDescriptor::new("effect", "effect", "effect", json!({})),
            PeakTool {
                active: active.clone(),
                peak: peak.clone(),
            },
        );
        binding.parallel_safe = parallel_safe;
        let graph = definition(
            vec![
                DagNode::react(WorkflowStage::new("a")),
                DagNode::react(WorkflowStage::new("b")),
            ],
            &[],
            2,
        );
        let a = TestBuilder::default()
            .plugin(StateStorePlugin(db.clone()))
            .plugin(ChatModelPlugin::new(ToolModel))
            .plugin(AgentLoopPlugin::new(DagWorkflowLoop::new(graph).unwrap()))
            .plugin(ToolProviderPlugin::new(
                "tools",
                StaticToolProvider::new(vec![binding]),
            ))
            .build()
            .await
            .unwrap();
        let mut request = RunRequest::new("tools", "input");
        request.limits.max_parallel_tools = slots;
        let h = a.start(request).unwrap();
        assert!(a.steer(&h, "unaddressed instruction").is_err());
        assert!(
            a.steer(&a.control(&h).unwrap(), "also unaddressed")
                .is_err()
        );
        assert_eq!(a.wait(h).await.unwrap().status, RunStatus::Completed);
        assert_eq!(peak.load(Ordering::SeqCst), expected_peak);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        a.framework().close().await;
        db.close().await.unwrap();
    }
}

#[tokio::test]
async fn react_step_and_tool_admission_limits_are_shared_across_nodes() {
    for (steps, tools, expected) in [
        (1, 10, RunStatus::StepLimit),
        (10, 1, RunStatus::ToolCallLimit),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let db = store(&temp.path().join("admission.db")).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let graph = definition(
            vec![
                DagNode::react(WorkflowStage::new("a")),
                DagNode::react(WorkflowStage::new("b")),
            ],
            &[],
            2,
        );
        let a = TestBuilder::default()
            .plugin(StateStorePlugin(db.clone()))
            .plugin(ChatModelPlugin::new(ToolModel))
            .plugin(AgentLoopPlugin::new(DagWorkflowLoop::new(graph).unwrap()))
            .plugin(ToolProviderPlugin::new(
                "tools",
                StaticToolProvider::new(vec![ToolBinding::new(
                    ToolDescriptor::new("effect", "effect", "effect", json!({})),
                    Effect {
                        calls: calls.clone(),
                        entered: Arc::new(Notify::new()),
                        wait: false,
                    },
                )]),
            ))
            .build()
            .await
            .unwrap();
        let mut request = RunRequest::new("admission", "input");
        request.limits.max_steps = steps;
        request.limits.max_tool_calls = tools;
        let h = a.start(request).unwrap();
        let id = h.run_id;
        assert_eq!(a.wait(h).await.unwrap().status, expected);
        let s = state(db.as_ref(), id).await;
        assert!(s.model_steps <= steps);
        assert!(s.tool_calls <= tools);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            db.load(id).await.unwrap().status,
            ExecutionStatus::Completed
        );
        assert_eq!(
            a.run(resume("admission", id)).await.unwrap().status,
            expected
        );
        a.framework().close().await;
        db.close().await.unwrap();
    }
}
