//! Kills a separate executor process, then recovers through a new connection and
//! Agent. Unlike cancelling a future, this bypasses every Rust Drop/cleanup path.
use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_state_sqlite::*;
use eliteforge_agent_test_support::*;
use serde_json::{Value, json};
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

fn definition() -> WorkflowDefinition {
    WorkflowDefinition {
        id: "crash-fixture".into(),
        version: 1,
        stages: vec![WorkflowStage::new("execute")],
    }
}
fn marker(path: &Path, value: &[u8]) {
    let mut file = std::fs::File::create(path).unwrap();
    file.write_all(value).unwrap();
    file.sync_all().unwrap();
}
struct CrashModel {
    directory: PathBuf,
    mode: String,
}
#[async_trait]
impl ChatModel for CrashModel {
    async fn generate(
        &self,
        _model_id: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        if self.mode == "model" {
            marker(&self.directory.join("ready"), b"provider entered");
            std::future::pending().await
        } else if self.mode == "offline" {
            panic!("terminal replay must not invoke a model")
        } else if request.messages.iter().any(|m| m.role == Role::Tool) {
            Ok(ChatResponse::text("reconciled"))
        } else {
            Ok(ChatResponse {
                tool_calls: vec![ToolCall::new("call", "effect", json!({}))],
                usage: Usage {
                    input_tokens: 2,
                    output_tokens: 2,
                },
                ..Default::default()
            })
        }
    }
}
struct CrashTool(PathBuf);
#[async_trait]
impl Tool for CrashTool {
    async fn call(&self, _: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        // create_new detects an accidental second external side effect.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.0.join("external-effect"))
            .unwrap();
        file.write_all(b"exactly one effect").unwrap();
        file.sync_all().unwrap();
        marker(&self.0.join("ready"), b"effect committed");
        std::future::pending().await
    }
}
struct OfflineTools;
#[async_trait]
impl ToolProvider for OfflineTools {
    async fn lease(&self, _: &RunContext) -> Result<ToolLease, AgentError> {
        Err(AgentError::ToolSetup("remote tools offline".into()))
    }
}
async fn open(directory: &Path) -> Arc<SqliteStateStore> {
    Arc::new(
        SqliteStateStore::open(directory.join("state.db"), SqliteOptions::default())
            .await
            .unwrap(),
    )
}
async fn agent(directory: &Path, mode: &str, store: Arc<SqliteStateStore>) -> Agent {
    let builder = TestBuilder::default()
        .plugin(ChatModelPlugin::new(CrashModel {
            directory: directory.into(),
            mode: mode.into(),
        }))
        .plugin(StateStorePlugin(store))
        .plugin(AgentLoopPlugin::new(
            WorkflowLoop::new(definition())
                .unwrap()
                .with_lease_duration(Duration::from_millis(200))
                .unwrap(),
        ));
    if mode == "offline" {
        builder
            .plugin(ToolProviderPlugin::new("tools", OfflineTools))
            .build()
            .await
            .unwrap()
    } else {
        builder
            .plugin(ToolProviderPlugin::new(
                "tools",
                StaticToolProvider::new(vec![ToolBinding::new(
                    ToolDescriptor::new(
                        "effect",
                        "effect",
                        "crash fixture",
                        json!({"type":"object"}),
                    ),
                    CrashTool(directory.into()),
                )]),
            ))
            .build()
            .await
            .unwrap()
    }
}
fn resume(id: RunId) -> RunRequest {
    let mut request = RunRequest::new("s", "");
    request.resume = Some(ResumeRequest {
        run_id: id,
        response: None,
        resolutions: vec![],
    });
    request
}
async fn kill_at_boundary(directory: &Path, mode: &str) {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--executor", directory.to_str().unwrap(), mode])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .unwrap();
    let ready = tokio::time::timeout(Duration::from_secs(10), async {
        while !directory.join("ready").exists() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "executor exited before reaching boundary"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert!(!status.success());
    ready.unwrap();
    // The killed executor cannot release its lease; recovery must wait for expiry.
    tokio::time::sleep(Duration::from_millis(250)).await;
}
async fn effect_survives_process_death_without_automatic_reexecution() {
    let dir = tempfile::tempdir().unwrap();
    kill_at_boundary(dir.path(), "effect").await;
    let store = open(dir.path()).await;
    let saved = store.latest("s").await.unwrap().unwrap();
    let executions = saved.state["cursor"]["Tools"]["executions"]
        .as_array()
        .unwrap();
    assert_eq!(executions[0]["state"], "Started");
    let invocation_id: RunId =
        serde_json::from_value(executions[0]["invocation_id"].clone()).unwrap();
    let executor = agent(dir.path(), "effect", store.clone()).await;
    assert_eq!(
        executor.run(resume(saved.run_id)).await.unwrap().status,
        RunStatus::RecoveryRequired
    );
    let mut reconciliation = resume(saved.run_id);
    reconciliation
        .resume
        .as_mut()
        .unwrap()
        .resolutions
        .push(CallResolution::Record {
            invocation_id,
            output: ToolOutput::text("external effect verified"),
        });
    assert_eq!(
        executor.run(reconciliation).await.unwrap().output,
        "reconciled"
    );
    assert_eq!(
        std::fs::read(dir.path().join("external-effect")).unwrap(),
        b"exactly one effect"
    );
    executor.close().await;
    store.close().await.unwrap();
    let store = open(dir.path()).await;
    let offline = agent(dir.path(), "offline", store.clone()).await;
    assert_eq!(
        offline.run(resume(saved.run_id)).await.unwrap().output,
        "reconciled"
    );
    offline.close().await;
    store.close().await.unwrap();
}
async fn in_flight_model_reservation_survives_process_death() {
    let dir = tempfile::tempdir().unwrap();
    kill_at_boundary(dir.path(), "model").await;
    let store = open(dir.path()).await;
    let saved = store.latest("s").await.unwrap().unwrap();
    let charged = saved.state["budget_usage"]["input_tokens"]
        .as_u64()
        .unwrap()
        + saved.state["budget_usage"]["output_tokens"]
            .as_u64()
            .unwrap();
    assert_eq!(charged, 512);
    let executor = agent(dir.path(), "offline", store.clone()).await;
    // For this nonterminal case use available tools, but a model which panics if
    // called: the restored exhausted ledger must reject it before dispatch.
    executor.close().await;
    let executor = TestBuilder::default()
        .plugin(ChatModelPlugin::new(CrashModel {
            directory: dir.path().into(),
            mode: "offline".into(),
        }))
        .plugin(StateStorePlugin(store.clone()))
        .plugin(AgentLoopPlugin::new(
            WorkflowLoop::new(definition()).unwrap(),
        ))
        .build()
        .await
        .unwrap();
    let error = executor.run(resume(saved.run_id)).await.unwrap_err();
    assert!(error.to_string().contains("budget exhausted"), "{error}");
    executor.close().await;
    store.close().await.unwrap();
}

struct JsonNode(PathBuf);
#[async_trait]
impl WorkflowNode for JsonNode {
    fn kind(&self) -> &str {
        "crash.json"
    }
    async fn execute(
        &self,
        _: WorkflowNodeInput,
        _: &WorkflowNodeContext,
        _: &AgentServices,
    ) -> Result<WorkflowNodeResult, AgentError> {
        // A completed predecessor must never execute again after process death.
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(self.0.join("json-node-effect"))
            .unwrap();
        file.write_all(b"one execution").unwrap();
        file.sync_all().unwrap();
        Ok(WorkflowNodeResult::completed(
            json!({"nested":{"answer":42},"list":[1,2,3]}),
        ))
    }
}
async fn dag_agent(directory: &Path, mode: &str, store: Arc<SqliteStateStore>) -> Agent {
    let graph = DagWorkflowDefinition {
        id: "crash-dag".into(),
        version: 1,
        nodes: vec![
            DagNode::new("json", "crash.json", Value::Null),
            DagNode::react(WorkflowStage::new("react")),
        ],
        edges: vec![DagEdge::new("json", "react")],
        max_parallel_nodes: 2,
    };
    TestBuilder::default()
        .plugin(StateStorePlugin(store))
        .plugin(ChatModelPlugin::new(CrashModel {
            directory: directory.into(),
            mode: mode.into(),
        }))
        .plugin(WorkflowNodePlugin::new("json", JsonNode(directory.into())))
        .plugin(AgentLoopPlugin::new(
            DagWorkflowLoop::new(graph)
                .unwrap()
                .with_lease_duration(Duration::from_millis(200))
                .unwrap(),
        ))
        .plugin(ToolProviderPlugin::new(
            "tools",
            StaticToolProvider::new(vec![ToolBinding::new(
                ToolDescriptor::new(
                    "effect",
                    "effect",
                    "crash fixture",
                    json!({"type":"object"}),
                ),
                CrashTool(directory.into()),
            )]),
        ))
        .build()
        .await
        .unwrap()
}
async fn dag_process_crash_preserves_json_tools_and_budget(mode: &str) {
    let dir = tempfile::tempdir().unwrap();
    kill_at_boundary(dir.path(), &format!("dag-{mode}")).await;
    let store = open(dir.path()).await;
    let saved = store.latest("s").await.unwrap().unwrap();
    let state: DagWorkflowState = serde_json::from_value(saved.state).unwrap();
    assert_eq!(state.nodes["json"].status, WorkflowNodeStatus::Succeeded);
    assert_eq!(
        state.nodes["json"].result.as_ref().unwrap().value,
        json!({"nested":{"answer":42},"list":[1,2,3]})
    );
    let inner: Checkpoint =
        serde_json::from_value(state.nodes["react"].checkpoint.clone().unwrap()).unwrap();
    let inner: WorkflowState = serde_json::from_value(inner.state).unwrap();
    let executor = dag_agent(
        dir.path(),
        if mode == "model" { "offline" } else { "effect" },
        store.clone(),
    )
    .await;
    if mode == "effect" {
        assert_eq!(
            executor.run(resume(saved.run_id)).await.unwrap().status,
            RunStatus::RecoveryRequired
        );
        let WorkflowCursor::Tools { executions, .. } = inner.cursor else {
            panic!("missing tool intent")
        };
        let mut r = resume(saved.run_id);
        r.resume
            .as_mut()
            .unwrap()
            .resolutions
            .push(CallResolution::Record {
                invocation_id: executions[0].invocation_id,
                output: ToolOutput::text("verified"),
            });
        assert_eq!(executor.run(r).await.unwrap().output, "reconciled");
        assert_eq!(
            std::fs::read(dir.path().join("external-effect")).unwrap(),
            b"exactly one effect"
        );
    } else {
        assert_eq!(
            state.budget_usage.input_tokens + state.budget_usage.output_tokens,
            512
        );
        let error = executor.run(resume(saved.run_id)).await.unwrap_err();
        assert!(error.to_string().contains("budget exhausted"), "{error}");
    }
    executor.close().await;
    store.close().await.unwrap();
}
#[tokio::main]
async fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).is_some_and(|arg| arg == "--executor") {
        let directory = Path::new(&args[2]);
        let mode = &args[3];
        let store = open(directory).await;
        let executor = if let Some(mode) = mode.strip_prefix("dag-") {
            dag_agent(directory, mode, store).await
        } else {
            agent(directory, mode, store).await
        };
        let mut request = RunRequest::new("s", "perform operation");
        if mode == "model" || mode == "dag-model" {
            request.limits.max_total_tokens = 512;
        }
        executor.run(request).await.unwrap();
        panic!("executor must be killed at the boundary");
    }
    tokio::time::timeout(Duration::from_secs(30), async {
        effect_survives_process_death_without_automatic_reexecution().await;
        in_flight_model_reservation_survives_process_death().await;
        dag_process_crash_preserves_json_tools_and_budget("effect").await;
        dag_process_crash_preserves_json_tools_and_budget("model").await;
    })
    .await
    .unwrap();
    println!(
        "Crash recovery scenarios: 4 passed (serial and DAG effects, JSON results and durable model budget)"
    );
}
