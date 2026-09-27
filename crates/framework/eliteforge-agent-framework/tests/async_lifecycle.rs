use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_test_support::*;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

struct PendingModel(Arc<Notify>);
#[async_trait]
impl ChatModel for PendingModel {
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
struct ShutdownPlugin {
    entered: Arc<Notify>,
    release: Arc<Notify>,
    count: Arc<AtomicUsize>,
}
#[async_trait]
impl Plugin for ShutdownPlugin {
    fn id(&self) -> &str {
        "shutdown"
    }
    async fn install(&self, ctx: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let entered = self.entered.clone();
        let release = self.release.clone();
        let count = self.count.clone();
        ctx.on_shutdown(move || async move {
            entered.notify_one();
            release.notified().await;
            count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        Ok(())
    }
}
#[tokio::test]
async fn cancelled_close_waiter_does_not_abandon_shutdown() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let count = Arc::new(AtomicUsize::new(0));
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(PendingModel(Arc::new(Notify::new()))))
        .plugin(ShutdownPlugin {
            entered: entered.clone(),
            release: release.clone(),
            count: count.clone(),
        })
        .build()
        .await
        .unwrap();
    let closing = agent.clone();
    let first = tokio::spawn(async move { closing.close().await });
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    first.abort();
    let _ = first.await;
    release.notify_one();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), agent.close())
            .await
            .unwrap()
            .is_success()
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert!(matches!(
        agent.start(RunRequest::new("s", "x")),
        Err(AgentError::Closed)
    ));
}
struct AsyncTools(Arc<AtomicUsize>);
#[async_trait]
impl ToolProvider for AsyncTools {
    async fn lease(&self, _: &RunContext) -> Result<ToolLease, AgentError> {
        let count = self.0.clone();
        Ok(ToolLease::new(vec![]).with_shutdown(move || async move {
            tokio::task::yield_now().await;
            count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }))
    }
}
#[tokio::test]
async fn cancelled_loop_awaits_tool_resource_shutdown_before_result() {
    let entered = Arc::new(Notify::new());
    let count = Arc::new(AtomicUsize::new(0));
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(PendingModel(entered.clone())))
        .plugin(ToolProviderPlugin::new(
            "resources",
            AsyncTools(count.clone()),
        ))
        .build()
        .await
        .unwrap();
    let handle = agent.start(RunRequest::new("s", "x")).unwrap();
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    agent.cancel(&handle).unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), agent.wait(handle))
            .await
            .unwrap(),
        Err(AgentError::Cancelled)
    ));
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert!(agent.close().await.is_success());
}
#[tokio::test]
async fn framework_validation_failure_after_install_awaits_plugin_shutdown() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let count = Arc::new(AtomicUsize::new(0));
    release.notify_one();
    let result = TestBuilder::default()
        .default_models(false)
        .plugin(ShutdownPlugin {
            entered,
            release,
            count: count.clone(),
        })
        .build()
        .await;
    assert!(matches!(
        result,
        Err(AgentError::MissingService("ChatModel"))
    ));
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

struct FailedCleanupLoop(bool);
#[async_trait]
impl AgentLoop for FailedCleanupLoop {
    async fn run(
        &self,
        _: &RunRequest,
        context: &RunContext,
        _: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let mut scope = eliteforge_agent_plugin_core::ResourceScope::default();
        scope.on_shutdown(|| async { Err("resource cleanup failed".into()) });
        context.own_resources(scope);
        if self.0 {
            Err(AgentError::Cancelled)
        } else {
            Err(AgentError::Run("execution failed".into()))
        }
    }
}
struct FailedAfter;
#[async_trait]
impl RunMiddleware for FailedAfter {
    async fn intercept(
        &self,
        _: &RunRequest,
        _: &RunContext,
        next: RunNext<'_>,
    ) -> Result<RunResult, AgentError> {
        finish_with_cleanup(
            next.run().await,
            Err(AgentError::Run("middleware cleanup failed".into())),
        )
    }
}

#[tokio::test]
async fn resource_and_middleware_cleanup_failures_preserve_the_primary_error() {
    for cancelled in [false, true] {
        let agent = TestBuilder::default()
            .plugin(AgentLoopPlugin::new(FailedCleanupLoop(cancelled)))
            .plugin(RunMiddlewarePlugin::new("failed-after", FailedAfter))
            .build()
            .await
            .unwrap();
        let handle = agent.start(RunRequest::new("cleanup", "input")).unwrap();
        let mut events = agent.subscribe(&handle).unwrap();
        let error = agent.wait(handle).await.unwrap_err();
        assert_eq!(matches!(error.primary(), AgentError::Cancelled), cancelled);
        let AgentError::WithCleanup { primary, cleanup } = &error else {
            panic!("missing middleware error: {error}");
        };
        assert!(
            matches!(cleanup.as_ref(), AgentError::Run(message) if message == "middleware cleanup failed")
        );
        let AgentError::WithCleanup { primary, cleanup } = primary.as_ref() else {
            panic!("missing resource error: {error}");
        };
        assert!(
            matches!(cleanup.as_ref(), AgentError::Run(message) if message.contains("resource cleanup failed"))
        );
        if !cancelled {
            assert!(
                matches!(primary.as_ref(), AgentError::Run(message) if message == "execution failed")
            );
        }
        let mut terminals = 0;
        while let Ok(event) = events.try_recv() {
            match event.event {
                AgentEvent::RunCancelled => {
                    assert!(cancelled);
                    terminals += 1;
                }
                AgentEvent::RunFailed(message) => {
                    assert!(!cancelled);
                    assert!(message.contains("execution failed"));
                    assert!(message.contains("resource cleanup failed"));
                    terminals += 1;
                }
                _ => {}
            }
        }
        assert_eq!(terminals, 1);
        agent.close().await;
    }
}

struct FailedFinishMemory;
struct FailedFinishSession(Vec<Message>);
#[async_trait]
impl ConversationStore for FailedFinishMemory {
    async fn open(&self, _: &str) -> Result<Box<dyn ConversationSession>, ConversationError> {
        Ok(Box::new(FailedFinishSession(Vec::new())))
    }
}
#[async_trait]
impl ConversationSession for FailedFinishSession {
    async fn snapshot(&mut self) -> Result<ConversationSnapshot, ConversationError> {
        Ok(ConversationSnapshot::new(
            ConversationKey::new("failed-finish"),
            "fixture",
            self.0.clone(),
        ))
    }

    async fn append(&mut self, messages: &[Message]) -> Result<(), ConversationError> {
        self.0.extend_from_slice(messages);
        Ok(())
    }
    async fn finish(&mut self, _: ConversationOutcome) -> Result<(), ConversationError> {
        Err(ConversationError::Other("memory finish failed".into()))
    }
}
struct FailedModel;
#[async_trait]
impl ChatModel for FailedModel {
    async fn generate(
        &self,
        _model_id: &str,
        _: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        Err(ModelError::Fatal("generation failed".into()))
    }
}
#[tokio::test]
async fn memory_finish_failure_is_attached_to_the_original_model_error() {
    let agent = TestBuilder::default()
        .plugin(ConversationStorePlugin::new(FailedFinishMemory))
        .plugin(ChatModelPlugin::new(FailedModel))
        .build()
        .await
        .unwrap();
    let error = agent
        .run(RunRequest::new("memory", "input"))
        .await
        .unwrap_err();
    let AgentError::WithCleanup { primary, cleanup } = error else {
        panic!("both model and memory failures required");
    };
    assert!(
        matches!(*primary, AgentError::Model(ModelError::Fatal(message)) if message == "generation failed")
    );
    assert!(
        matches!(*cleanup, AgentError::ConversationStore(ConversationError::Other(message)) if message == "memory finish failed")
    );
    agent.close().await;
}
