use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_test_support::*;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

fn completed(text: &str) -> RunResult {
    RunResult {
        output: text.into(),
        content: text.into(),
        structured: None,
        interactions: vec![],
        status: RunStatus::Completed,
        usage: Usage::default(),
        steps: 0,
        tool_calls: 0,
        external_session: None,
    }
}
struct SimpleLoop(Arc<Mutex<Vec<String>>>);
#[async_trait]
impl AgentLoop for SimpleLoop {
    async fn run(
        &self,
        _: &RunRequest,
        _: &RunContext,
        _: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        self.0.lock().unwrap().push("execute".into());
        Ok(completed("ok"))
    }
}
struct Trace(Arc<Mutex<Vec<String>>>);
#[async_trait]
impl RunLifecycleHook for Trace {
    async fn on_started(&self, context: &RunLifecycleContext<'_>) {
        assert!(!context.execution_id.is_nil());
        self.0.lock().unwrap().push("started".into());
    }
    async fn on_finished(
        &self,
        _: &RunLifecycleContext<'_>,
        result: &Result<RunResult, AgentError>,
    ) {
        self.0.lock().unwrap().push(
            if result.is_ok() {
                "finished.ok"
            } else {
                "finished.error"
            }
            .into(),
        );
    }
}
#[async_trait]
impl RunMiddleware for Trace {
    async fn intercept(
        &self,
        _: &RunRequest,
        _: &RunContext,
        next: RunNext<'_>,
    ) -> Result<RunResult, AgentError> {
        self.0.lock().unwrap().push("outer.before".into());
        let result = next.run().await;
        self.0.lock().unwrap().push("outer.after".into());
        result
    }
}
struct BothPlugin(Arc<Trace>);
#[async_trait]
impl Plugin for BothPlugin {
    fn id(&self) -> &str {
        "both"
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        context.contribute(RunMiddlewareService(self.0.clone()))?;
        context.contribute(RunLifecycleHookService(self.0.clone()))
    }
}
struct Inner {
    log: Arc<Mutex<Vec<String>>>,
    deny: bool,
}
#[async_trait]
impl RunMiddleware for Inner {
    async fn intercept(
        &self,
        _: &RunRequest,
        _: &RunContext,
        next: RunNext<'_>,
    ) -> Result<RunResult, AgentError> {
        self.log.lock().unwrap().push("inner.before".into());
        if self.deny {
            return Err(AgentError::Run("denied".into()));
        }
        let result = next.run().await;
        self.log.lock().unwrap().push("inner.after".into());
        result
    }
}
#[tokio::test]
async fn one_plugin_contributes_hooks_and_interceptors_with_distinct_order_and_denial_semantics() {
    for deny in [false, true] {
        let log = Arc::new(Mutex::new(vec![]));
        let a = TestBuilder::default()
            .plugin(BothPlugin(Arc::new(Trace(log.clone()))))
            .plugin(RunMiddlewarePlugin::new(
                "inner",
                Inner {
                    log: log.clone(),
                    deny,
                },
            ))
            .plugin(AgentLoopPlugin::new(SimpleLoop(log.clone())))
            .build()
            .await
            .unwrap();
        let result = a.run(RunRequest::new("s", "input")).await;
        assert_eq!(result.is_err(), deny);
        let expected = if deny {
            vec![
                "started",
                "outer.before",
                "inner.before",
                "outer.after",
                "finished.error",
            ]
        } else {
            vec![
                "started",
                "outer.before",
                "inner.before",
                "execute",
                "inner.after",
                "outer.after",
                "finished.ok",
            ]
        };
        assert_eq!(*log.lock().unwrap(), expected);
        a.framework().close().await;
    }
}
struct BadHook(bool);
#[async_trait]
impl RunLifecycleHook for BadHook {
    async fn on_started(&self, _: &RunLifecycleContext<'_>) {
        if self.0 {
            panic!("hook fixture panic");
        }
        std::future::pending::<()>().await;
    }
    async fn on_finished(
        &self,
        context: &RunLifecycleContext<'_>,
        _: &Result<RunResult, AgentError>,
    ) {
        self.on_started(context).await;
    }
}
#[tokio::test]
async fn lifecycle_hook_panics_and_timeouts_are_reported_without_changing_result() {
    let log = Arc::new(Mutex::new(vec![]));
    let a = TestBuilder::default()
        .plugin(RunLifecycleHookPlugin::new("panic", BadHook(true)))
        .plugin(RunLifecycleHookPlugin::new("timeout", BadHook(false)))
        .plugin(RunLifecycleHookPlugin::new("trace", Trace(log.clone())))
        .plugin(AgentLoopPlugin::new(SimpleLoop(log.clone())))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("s", "input");
    request.limits.cleanup_timeout = Duration::from_millis(10);
    let handle = a.start(request).unwrap();
    let mut events = a.subscribe(&handle).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(2), a.wait(handle))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.output, "ok");
    assert_eq!(*log.lock().unwrap(), ["started", "execute", "finished.ok"]);
    let mut failures = vec![];
    while let Ok(envelope) = events.try_recv() {
        if let AgentEvent::LifecycleHookFailed {
            phase,
            hook_index,
            failure,
        } = envelope.event
        {
            failures.push((phase, hook_index, failure));
        }
    }
    assert_eq!(failures.len(), 4);
    assert!(matches!(
        failures[0],
        (
            RunLifecyclePhase::Started,
            0,
            LifecycleHookFailure::Panicked
        )
    ));
    assert!(matches!(
        failures[3],
        (
            RunLifecyclePhase::Finished,
            1,
            LifecycleHookFailure::TimedOut
        )
    ));
    a.framework().close().await;
}

struct Effect(Arc<AtomicUsize>);
#[async_trait]
impl Tool for Effect {
    async fn call(&self, _: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::text("effect"))
    }
}
struct InvokeTool {
    calls: Arc<AtomicUsize>,
    observed: Arc<Mutex<Option<Result<ToolOutput, ToolError>>>>,
}
#[async_trait]
impl AgentLoop for InvokeTool {
    async fn run(
        &self,
        _: &RunRequest,
        context: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let binding = ToolBinding::new(
            ToolDescriptor::new("tool", "tool", "", json!({})),
            Effect(self.calls.clone()),
        );
        let result = services
            .tool_invoker
            .invoke(
                "invocation",
                &ToolCall::new("call", "tool", json!({})),
                &binding,
                context,
                Duration::from_millis(20),
            )
            .await;
        *self.observed.lock().unwrap() = Some(result);
        Ok(completed("done"))
    }
}
struct Recover;
#[async_trait]
impl ToolMiddleware for Recover {
    async fn intercept(
        &self,
        _: &ToolInvocationContext<'_>,
        next: ToolNext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let _ = next.run().await;
        Ok(ToolOutput::text("pretend success"))
    }
}
struct ToolControl(&'static str);
#[async_trait]
impl ToolMiddleware for ToolControl {
    async fn intercept(
        &self,
        _: &ToolInvocationContext<'_>,
        next: ToolNext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        match self.0 {
            "deny" => Err(ToolError::Failed("denied".into())),
            "late" => {
                tokio::time::sleep(Duration::from_millis(25)).await;
                next.run().await
            }
            "post" => {
                next.run().await?;
                Err(ToolError::Failed("post failed".into()))
            }
            "panic" => {
                next.run().await?;
                panic!("post panic");
            }
            _ => unreachable!(),
        }
    }
}
#[tokio::test]
async fn tool_denials_deadlines_and_post_dispatch_failures_cannot_be_hidden_by_outer_middleware() {
    for mode in ["deny", "late", "post", "panic"] {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::new(Mutex::new(None));
        let a = TestBuilder::default()
            .plugin(ToolMiddlewarePlugin::new("recover", Recover))
            .plugin(ToolMiddlewarePlugin::new("control", ToolControl(mode)))
            .plugin(AgentLoopPlugin::new(InvokeTool {
                calls: calls.clone(),
                observed: observed.clone(),
            }))
            .build()
            .await
            .unwrap();
        a.run(RunRequest::new("s", "input")).await.unwrap();
        let error = observed.lock().unwrap().take().unwrap().unwrap_err();
        if mode == "deny" || mode == "late" {
            assert!(matches!(error, ToolError::Failed(_)), "{mode}: {error}");
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        } else {
            assert!(
                matches!(error, ToolError::Indeterminate(_)),
                "{mode}: {error}"
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
        a.framework().close().await;
    }
}
