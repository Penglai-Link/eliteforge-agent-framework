use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_tools_local::*;
use eliteforge_agent_test_support::TestBuilder;
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};
struct Scope(std::path::PathBuf);
#[async_trait]
impl LocalToolScopeProvider for Scope {
    async fn grant(&self, _: &RunContext) -> Result<LocalToolGrant, AgentError> {
        Ok(LocalToolGrant {
            terminal_owner: None,
            filesystem: FileSystemToolsConfig::new(&self.0)
                .with_authorized_roots([self.0.clone()])
                .with_authorized_symlinks(true),
            commands: SystemCommandConfig::new().with_shell("/bin/sh"),
            tool_ids: BTreeSet::from(["framework.terminal.start".into()]),
            call_names: BTreeMap::new(),
            exposed_tool_ids: None,
        })
    }
}
struct Model;
#[async_trait]
impl ChatModel for Model {
    async fn generate(
        &self,
        _: &str,
        r: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        Ok(if r.messages.last().is_some_and(|m| m.role == Role::Tool) {
            ChatResponse::text("done")
        } else {
            ChatResponse::tool_calls(vec![ToolCall::new(
                "start",
                "terminal_start",
                json!({"command":"touch should-not-start; sleep 20"}),
            )])
        })
    }
}
struct OccupyWorker;
#[async_trait]
impl ToolMiddleware for OccupyWorker {
    async fn intercept(
        &self,
        _: &ToolInvocationContext<'_>,
        next: ToolNext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let (started, ready) = tokio::sync::oneshot::channel();
        tokio::task::spawn_blocking(move || {
            let _ = started.send(());
            std::thread::sleep(Duration::from_millis(150));
        });
        ready.await.map_err(|e| ToolError::Failed(e.to_string()))?;
        next.run().await
    }
}
#[test]
fn per_tool_timeout_abandons_a_queued_native_pty_start() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap()
        .block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap();
            let service = pty::PtyService::default();
            let agent = TestBuilder::default()
                .plugin(ChatModelPlugin::new(Model))
                .plugin(
                    ScopedLocalToolsPlugin::new(Arc::new(Scope(root.clone())))
                        .with_pty(service.clone()),
                )
                .plugin(ToolMiddlewarePlugin::new(
                    "test.blocking-worker",
                    OccupyWorker,
                ))
                .build()
                .await
                .unwrap();
            let mut request = RunRequest::new("timeout", "start");
            request.limits.tool_timeout = Duration::from_millis(40);
            request.limits.run_timeout = Duration::from_secs(3);
            let error = agent.run(request).await.unwrap_err();
            assert!(matches!(error.primary(), AgentError::Run(message)
                if message.contains("tool effects are indeterminate") && message.contains("tool timed out")));
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert!(!root.join("should-not-start").exists());
            assert!(service.list("timeout").unwrap().is_empty());
            assert!(agent.close().await.is_success());
        });
}

struct CommandScope(std::path::PathBuf);
#[async_trait]
impl LocalToolScopeProvider for CommandScope {
    async fn grant(&self, _: &RunContext) -> Result<LocalToolGrant, AgentError> {
        Ok(LocalToolGrant {
            terminal_owner: None,
            filesystem: FileSystemToolsConfig::new(&self.0)
                .with_authorized_roots([self.0.clone()])
                .with_authorized_symlinks(true),
            commands: SystemCommandConfig::new()
                .with_shell("/bin/sh")
                .with_async_after(Duration::from_secs(1)),
            tool_ids: BTreeSet::from(["system.command.exec".into()]),
            call_names: BTreeMap::new(),
            exposed_tool_ids: None,
        })
    }
}
struct CommandModel;
#[async_trait]
impl ChatModel for CommandModel {
    async fn generate(
        &self,
        _: &str,
        r: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        Ok(if r.messages.last().is_some_and(|m| m.role == Role::Tool) {
            ChatResponse::text("done")
        } else {
            ChatResponse::tool_calls(vec![ToolCall::new(
                "command",
                "system_exec",
                json!({"command":"sleep 0.3; touch late-command"}),
            )])
        })
    }
}
#[tokio::test]
async fn per_tool_timeout_stops_command_before_a_task_id_is_accepted() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(CommandModel))
        .plugin(ScopedLocalToolsPlugin::new(Arc::new(CommandScope(
            root.clone(),
        ))))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("command-timeout", "run");
    request.limits.tool_timeout = Duration::from_millis(40);
    request.limits.run_timeout = Duration::from_secs(3);
    let error = agent.run(request).await.unwrap_err();
    assert!(matches!(error.primary(), AgentError::Run(message)
                if message.contains("tool effects are indeterminate") && message.contains("tool timed out")));
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!root.join("late-command").exists());
    assert!(agent.close().await.is_success());
}
