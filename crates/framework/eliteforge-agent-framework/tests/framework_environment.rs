use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_storage_local::LocalStoragePlugin;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio_util::sync::CancellationToken;

struct Probe(&'static str);
#[async_trait]
impl AgentLoop for Probe {
    async fn run(
        &self,
        _: &RunRequest,
        context: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let mut ids = Vec::new();
        for provider in &services.tools {
            let mut lease = provider.lease(context).await?;
            ids.extend(lease.tools.iter().map(|tool| tool.descriptor.id.clone()));
            lease.close(std::time::Duration::from_secs(1)).await;
        }
        ids.sort();
        let output = format!("{}:{}", self.0, ids.join(","));
        Ok(RunResult {
            content: output.clone().into(),
            output,
            structured: None,
            status: RunStatus::Completed,
            interactions: vec![],
            usage: Usage::default(),
            steps: 1,
            tool_calls: 0,
            external_session: None,
        })
    }
}
struct Tracker {
    installed: Arc<AtomicUsize>,
    closed: Arc<AtomicUsize>,
}
#[async_trait]
impl Plugin for Tracker {
    fn id(&self) -> &str {
        "tracker"
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        self.installed.fetch_add(1, Ordering::SeqCst);
        let closed = self.closed.clone();
        context.on_cleanup(move || {
            closed.fetch_add(1, Ordering::SeqCst);
        });
        Ok(())
    }
}
struct StoreIdentity;
#[async_trait]
impl AgentOperation for StoreIdentity {
    type Service = SessionStoreService;
    type Output = Arc<dyn SessionStore>;
    async fn execute(
        self,
        service: Arc<Self::Service>,
        _: CancellationToken,
    ) -> Result<Self::Output, AgentError> {
        Ok(service.0.clone())
    }
}
struct DummyTool;
#[async_trait]
impl Tool for DummyTool {
    async fn call(&self, _: serde_json::Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        unreachable!()
    }
}
fn tool(id: &str) -> ToolBinding {
    ToolBinding::new(
        ToolDescriptor::new(id, id, "test", serde_json::json!({"type":"object"})),
        DummyTool,
    )
}
fn select(id: &str) -> AgentSelection {
    AgentSelection {
        agent_loop: Some(id.into()),
        ..AgentSelection::default()
    }
}
fn storage(directory: &tempfile::TempDir) -> LocalStoragePlugin {
    let artifacts = tempfile::Builder::new()
        .prefix("artifacts")
        .tempdir_in(directory.path())
        .unwrap()
        .keep();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&artifacts, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    LocalStoragePlugin::new(directory.path().join("state.sqlite"), artifacts)
}
async fn environment(directory: &tempfile::TempDir) -> Framework {
    Framework::builder()
        .default_models(false)
        .plugin(storage(directory))
        .plugin(AgentLoopPlugin::named("loop.a", Probe("a")))
        .plugin(AgentLoopPlugin::named("loop.b", Probe("b")))
        .build()
        .await
        .unwrap()
}

#[tokio::test]
async fn agents_share_storage_and_agent_close_does_not_close_environment() {
    let directory = tempfile::tempdir().unwrap();
    let framework = environment(&directory).await;
    let a = framework.agent(select("loop.a")).build().await.unwrap();
    let b = framework.agent(select("loop.b")).build().await.unwrap();
    let first = a.execute(StoreIdentity).await.unwrap();
    let second = b.execute(StoreIdentity).await.unwrap();
    let shared = framework.execute(StoreIdentity).await.unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    assert!(Arc::ptr_eq(&first, &shared));
    let key = ConversationKey::new("conversation");
    a.append_conversation(&key, 0, vec![Message::user("hello")])
        .await
        .unwrap();
    assert_eq!(b.conversation(&key).await.unwrap().messages.len(), 1);
    a.close().await;
    assert!(!framework.is_closed());
    assert_eq!(
        b.run(RunRequest::new("b-session", ""))
            .await
            .unwrap()
            .output,
        "b:"
    );
    assert!(
        framework
            .execute(DeleteSession("conversation".into()))
            .await
            .unwrap()
    );
    assert!(b.conversation(&key).await.unwrap().messages.is_empty());
    assert!(framework.close().await.is_success());
    assert!(b.is_closed());
    assert!(matches!(
        b.start(RunRequest::new("closed", "")),
        Err(AgentError::Closed)
    ));
    assert!(matches!(
        framework
            .execute(DeleteSession("conversation".into()))
            .await,
        Err(AgentError::Closed)
    ));
}

#[tokio::test]
async fn selection_reconfiguration_does_not_reinstall_plugins_and_failed_selection_keeps_old_agent()
{
    let directory = tempfile::tempdir().unwrap();
    let installed = Arc::new(AtomicUsize::new(0));
    let closed = Arc::new(AtomicUsize::new(0));
    let framework = Framework::builder()
        .default_models(false)
        .plugin(storage(&directory))
        .plugin(Tracker {
            installed: installed.clone(),
            closed: closed.clone(),
        })
        .plugin(AgentLoopPlugin::named("a", Probe("a")))
        .plugin(AgentLoopPlugin::named("b", Probe("b")))
        .build()
        .await
        .unwrap();
    let agent = framework.agent(select("a")).build().await.unwrap();
    let other = framework.agent(select("a")).build().await.unwrap();
    agent
        .reconfigure(framework.agent(select("b")))
        .await
        .unwrap();
    assert_eq!(
        agent.run(RunRequest::new("one", "")).await.unwrap().output,
        "b:"
    );
    assert_eq!(
        other.run(RunRequest::new("two", "")).await.unwrap().output,
        "a:"
    );
    assert!(matches!(
        agent.reconfigure(framework.agent(select("missing"))).await,
        Err(ReconfigureError::Rejected(_))
    ));
    assert_eq!(
        agent
            .run(RunRequest::new("three", ""))
            .await
            .unwrap()
            .output,
        "b:"
    );
    assert_eq!(installed.load(Ordering::SeqCst), 1);
    assert_eq!(closed.load(Ordering::SeqCst), 0);
    agent.close().await;
    assert_eq!(closed.load(Ordering::SeqCst), 0);
    let (one, two) = tokio::join!(framework.close(), framework.close());
    assert!(one.is_success());
    assert_eq!(one, two);
    assert_eq!(closed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn ambiguous_missing_and_conflicting_bindings_fail_and_defaults_are_explicit() {
    let directory = tempfile::tempdir().unwrap();
    let framework = environment(&directory).await;
    assert!(
        framework
            .agent(AgentSelection::default())
            .build()
            .await
            .is_err()
    );
    assert!(framework.agent(select("missing")).build().await.is_err());
    let directory2 = tempfile::tempdir().unwrap();
    let other = environment(&directory2).await;
    let agent = framework.agent(select("loop.a")).build().await.unwrap();
    assert!(matches!(
        agent.reconfigure(other.agent(select("loop.b"))).await,
        Err(ReconfigureError::Invalid(_))
    ));
    framework.close().await;
    other.close().await;
    let framework = Framework::builder()
        .default_models(false)
        .plugin(AgentLoopPlugin::named("a", NoHistory))
        .plugin(AgentLoopPlugin::named("b", NoHistory))
        .defaults(select("b"))
        .build()
        .await
        .unwrap();
    let agent = framework
        .agent(AgentSelection::default())
        .build()
        .await
        .unwrap();
    agent.close().await;
    framework.close().await;
    assert!(
        Framework::builder()
            .default_models(false)
            .plugin(AgentLoopPlugin::named("a", NoHistory))
            .defaults(select("missing"))
            .build()
            .await
            .is_err()
    );
}
struct NoHistory;
#[async_trait]
impl AgentLoop for NoHistory {
    fn requires_conversations(&self) -> bool {
        false
    }
    async fn run(
        &self,
        _: &RunRequest,
        _: &RunContext,
        _: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        unreachable!()
    }
}

#[tokio::test]
async fn tools_are_opt_in_and_each_agent_gets_its_own_tool_set() {
    let directory = tempfile::tempdir().unwrap();
    let framework = Framework::builder()
        .default_models(false)
        .plugin(storage(&directory))
        .plugin(AgentLoopPlugin::named("probe", Probe("tools")))
        .plugin(ToolProviderPlugin::new(
            "tools",
            StaticToolProvider::new(vec![tool("read"), tool("write")]),
        ))
        .build()
        .await
        .unwrap();
    let none = framework.agent(select("probe")).build().await.unwrap();
    let read = framework
        .agent(AgentSelection {
            agent_loop: Some("probe".into()),
            tool_providers: Some(vec!["tools".into()]),
            tools: Some(vec!["read".into()]),
            ..AgentSelection::default()
        })
        .build()
        .await
        .unwrap();
    let all = framework
        .agent(AgentSelection {
            agent_loop: Some("probe".into()),
            tool_providers: Some(vec!["tools".into()]),
            ..AgentSelection::default()
        })
        .build()
        .await
        .unwrap();
    assert_eq!(
        none.run(RunRequest::new("none", "")).await.unwrap().output,
        "tools:"
    );
    assert_eq!(
        read.run(RunRequest::new("read", "")).await.unwrap().output,
        "tools:read"
    );
    assert_eq!(
        all.run(RunRequest::new("all", "")).await.unwrap().output,
        "tools:read,write"
    );
    assert!(
        framework
            .agent(AgentSelection {
                agent_loop: Some("probe".into()),
                tool_providers: Some(vec!["missing".into()]),
                ..AgentSelection::default()
            })
            .build()
            .await
            .is_err()
    );
    framework.close().await;
}

#[tokio::test]
async fn singleton_storage_and_static_tool_conflicts_fail_at_framework_build() {
    let directory = tempfile::tempdir().unwrap();
    assert!(
        Framework::builder()
            .default_models(false)
            .plugin(storage(&directory))
            .plugin(ConversationStorePlugin::new(
                eliteforge_agent_test_support::TestConversationStore::default()
            ))
            .build()
            .await
            .is_err()
    );
    assert!(
        Framework::builder()
            .default_models(false)
            .plugin(ToolProviderPlugin::new(
                "one",
                StaticToolProvider::new(vec![tool("same")])
            ))
            .plugin(ToolProviderPlugin::new(
                "two",
                StaticToolProvider::new(vec![tool("same")])
            ))
            .build()
            .await
            .is_err()
    );
    assert_eq!(
        <dyn SessionStore as ExtensionPoint>::BINDING,
        ExtensionBinding::Shared
    );
    assert_eq!(
        <dyn AgentLoop as ExtensionPoint>::BINDING,
        ExtensionBinding::SelectOne
    );
    assert_eq!(
        <dyn ToolProvider as ExtensionPoint>::BINDING,
        ExtensionBinding::Contributions
    );
}

struct OperationToken;
#[async_trait]
impl AgentOperation for OperationToken {
    type Service = SessionStoreService;
    type Output = CancellationToken;
    async fn execute(
        self,
        _: Arc<Self::Service>,
        cancellation: CancellationToken,
    ) -> Result<Self::Output, AgentError> {
        Ok(cancellation)
    }
}
#[tokio::test]
async fn operation_tokens_finish_without_cancelling_the_environment() {
    let directory = tempfile::tempdir().unwrap();
    let framework = environment(&directory).await;
    let token = framework.execute(OperationToken).await.unwrap();
    assert!(token.is_cancelled());
    assert!(!framework.is_closed());
    assert!(
        !framework
            .execute(DeleteSession("missing".into()))
            .await
            .unwrap()
    );
    framework.close().await;
}

struct BlockingLoop {
    started: Arc<tokio::sync::Notify>,
    released: Arc<AtomicUsize>,
}
#[async_trait]
impl AgentLoop for BlockingLoop {
    fn requires_conversations(&self) -> bool {
        false
    }
    fn cooperative_cancellation(&self) -> bool {
        true
    }
    async fn run(
        &self,
        _: &RunRequest,
        context: &RunContext,
        _: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let released = self.released.clone();
        let mut resources = eliteforge_agent_plugin_core::ResourceScope::default();
        resources.on_cleanup(move || {
            released.fetch_add(1, Ordering::SeqCst);
        });
        context.own_resources(resources);
        self.started.notify_one();
        context.cancelled().await;
        Err(AgentError::Cancelled)
    }
}
#[tokio::test]
async fn framework_shutdown_drains_active_agents_before_plugin_cleanup() {
    struct CheckDrain {
        released: Arc<AtomicUsize>,
        closed: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Plugin for CheckDrain {
        fn id(&self) -> &str {
            "check-drain"
        }
        async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
            let released = self.released.clone();
            let closed = self.closed.clone();
            context.on_shutdown(move || async move {
                if released.load(Ordering::SeqCst) != 2 {
                    return Err("Agents were not drained".into());
                }
                closed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });
            Ok(())
        }
    }
    let started = Arc::new(tokio::sync::Notify::new());
    let released = Arc::new(AtomicUsize::new(0));
    let closed = Arc::new(AtomicUsize::new(0));
    let framework = Framework::builder()
        .default_models(false)
        .plugin(AgentLoopPlugin::named(
            "blocking",
            BlockingLoop {
                started: started.clone(),
                released: released.clone(),
            },
        ))
        .plugin(CheckDrain {
            released: released.clone(),
            closed: closed.clone(),
        })
        .build()
        .await
        .unwrap();
    let a = framework.agent(select("blocking")).build().await.unwrap();
    let b = framework.agent(select("blocking")).build().await.unwrap();
    let handle_a = a.start(RunRequest::new("a", "")).unwrap();
    started.notified().await;
    let handle_b = b.start(RunRequest::new("b", "")).unwrap();
    started.notified().await;
    assert!(matches!(
        a.reconfigure(framework.agent(select("blocking"))).await,
        Err(ReconfigureError::Busy(_))
    ));
    // The public Agent can disappear while its RunHandle keeps execution alive.
    drop(b);
    let report = tokio::time::timeout(std::time::Duration::from_secs(3), framework.close())
        .await
        .unwrap();
    assert!(report.is_success(), "{report:?}");
    assert_eq!(released.load(Ordering::SeqCst), 2);
    assert_eq!(closed.load(Ordering::SeqCst), 1);
    assert!(a.wait(handle_a).await.is_err());
    drop(handle_b);
}

struct ValidationGate {
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}
#[async_trait]
impl AgentLoop for ValidationGate {
    fn requires_conversations(&self) -> bool {
        false
    }
    async fn validate_services_async(&self, _: &AgentServices) -> Result<(), AgentError> {
        self.started.notify_one();
        self.release.notified().await;
        Ok(())
    }
    async fn run(
        &self,
        request: &RunRequest,
        context: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        Probe("new").run(request, context, services).await
    }
}
#[tokio::test]
async fn framework_close_cancels_an_agent_build_waiting_for_validation() {
    let started = Arc::new(tokio::sync::Notify::new());
    let framework = Framework::builder()
        .default_models(false)
        .plugin(AgentLoopPlugin::named(
            "gated",
            ValidationGate {
                started: started.clone(),
                release: Arc::new(tokio::sync::Notify::new()),
            },
        ))
        .build()
        .await
        .unwrap();
    let builder = framework.agent(select("gated"));
    let building = tokio::spawn(async move { builder.build().await });
    started.notified().await;
    let report = tokio::time::timeout(std::time::Duration::from_secs(3), framework.close())
        .await
        .unwrap();
    assert!(report.is_success());
    assert!(matches!(building.await.unwrap(), Err(AgentError::Closed)));
}
#[tokio::test]
async fn aborting_reconfiguration_caller_does_not_leave_admission_suspended() {
    let directory = tempfile::tempdir().unwrap();
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let framework = Framework::builder()
        .default_models(false)
        .plugin(storage(&directory))
        .plugin(AgentLoopPlugin::named("old", Probe("old")))
        .plugin(AgentLoopPlugin::named(
            "new",
            ValidationGate {
                started: started.clone(),
                release: release.clone(),
            },
        ))
        .build()
        .await
        .unwrap();
    let agent = framework.agent(select("old")).build().await.unwrap();
    let caller_agent = agent.clone();
    let target = framework.agent(select("new"));
    let caller = tokio::spawn(async move { caller_agent.reconfigure(target).await });
    started.notified().await;
    caller.abort();
    let _ = caller.await;
    release.notify_one();
    let handle = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            match agent.start(RunRequest::new("after-update", "")) {
                Ok(handle) => break handle,
                Err(AgentError::Run(message)) if message.contains("update in progress") => {
                    tokio::task::yield_now().await
                }
                Err(error) => panic!("unexpected error: {error}"),
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(agent.wait(handle).await.unwrap().output, "new:");
    assert!(framework.close().await.is_success());
}
