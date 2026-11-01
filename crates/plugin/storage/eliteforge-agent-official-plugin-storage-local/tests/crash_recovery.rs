//! Kills a separate executor process, then checks persisted state through a new connection and
//! Agent. Unlike cancelling a future, this bypasses every Rust Drop/cleanup path.
use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_storage_local::*;
use eliteforge_agent_test_support::*;
use serde_json::{Value, json};
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

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
            Ok(ChatResponse::text("completed"))
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

fn resume(id: RunId) -> RunRequest {
    let mut request = RunRequest::new("s", "");
    request.resume = Some(ResumeRequest {
        run_id: id,
        response: None,
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
    let executor = agent(dir.path(), "effect", store.clone()).await;
    let error = executor.run(resume(saved.run_id)).await.unwrap_err();
    assert!(
        error.to_string().contains("tool execution was interrupted"),
        "{error}"
    );
    assert_eq!(
        store.load(saved.run_id).await.unwrap().status,
        ExecutionStatus::Failed
    );
    assert_eq!(
        std::fs::read(dir.path().join("external-effect")).unwrap(),
        b"exactly one effect"
    );
    let history = store
        .read_session("s", HistoryQuery::default())
        .await
        .unwrap();
    assert_eq!(history.records.tool_invocations.len(), 1);
    assert_eq!(
        history.records.tool_invocations[0].status,
        HistoryStatus::Unknown
    );
    assert_eq!(history.records.messages[1].status, HistoryStatus::Completed);
    executor.framework().close().await;
    store.close().await.unwrap();
    let store = open(dir.path()).await;
    let offline = agent(dir.path(), "offline", store.clone()).await;
    assert!(matches!(
        offline.run(resume(saved.run_id)).await,
        Err(AgentError::Run(_))
    ));
    offline.close().await;
    store.close().await.unwrap();
}

struct JsonNode(PathBuf);

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
