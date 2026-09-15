use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_test_support::*;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

#[derive(Default, Clone)]
struct Trace(Arc<Mutex<Vec<(RunId, RunLineage)>>>);
#[async_trait]
impl EventSink for Trace {
    async fn on_event(&self, context: &RunContext, event: &AgentEvent) {
        if matches!(event, AgentEvent::RunStarted { .. }) {
            self.0
                .lock()
                .unwrap()
                .push((context.run_id, context.lineage().clone()));
        }
    }
}
struct Echo;
#[async_trait]
impl ChatModel for Echo {
    async fn generate(
        &self,
        _model_id: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        Ok(ChatResponse {
            content: request.messages.last().unwrap().content.clone(),
            usage: Usage {
                input_tokens: 4,
                output_tokens: 2,
            },
            ..Default::default()
        })
    }
}
fn result(context: &RunContext, text: String) -> RunResult {
    RunResult {
        output: text.clone(),
        content: text.into(),
        structured: None,
        interactions: vec![],
        status: RunStatus::Completed,
        usage: context.usage(),
        steps: 0,
        tool_calls: 0,
    }
}
struct Delegator {
    child: Agent,
}
#[async_trait]
impl AgentLoop for Delegator {
    async fn run(
        &self,
        request: &RunRequest,
        context: &RunContext,
        _: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let mut child_request = RunRequest::new(
            format!("{}-child", request.session_id),
            request.input.clone(),
        );
        child_request.limits.max_total_tokens = 500;
        let answer = self.child.run_child(child_request, context).await?;
        Ok(result(context, answer.output))
    }
}
#[tokio::test]
async fn actual_nested_invokers_inherit_identity_and_charge_parent_budget() {
    let trace = Trace::default();
    let leaf = TestBuilder::default()
        .plugin(ChatModelPlugin::new(Echo))
        .plugin(EventPlugin::new("trace", trace.clone()))
        .build()
        .await
        .unwrap();
    let middle = TestBuilder::default()
        .plugin(AgentLoopPlugin::new(Delegator {
            child: leaf.clone(),
        }))
        .plugin(EventPlugin::new("trace", trace.clone()))
        .build()
        .await
        .unwrap();
    let root = TestBuilder::default()
        .plugin(AgentLoopPlugin::new(Delegator {
            child: middle.clone(),
        }))
        .plugin(EventPlugin::new("trace", trace.clone()))
        .build()
        .await
        .unwrap();
    for input in ["first task", "不同任务"] {
        let handle = root.start(RunRequest::new(input, input)).unwrap();
        let root_id = handle.run_id;
        let answer = root.wait(handle).await.unwrap();
        assert_eq!(answer.output, input);
        assert_eq!(answer.usage.input_tokens + answer.usage.output_tokens, 6);
        let events = trace.0.lock().unwrap();
        let tree: Vec<_> = events
            .iter()
            .filter(|(_, l)| l.root_run_id == root_id)
            .collect();
        assert_eq!(tree.len(), 3);
        let parent = tree.iter().find(|(_, l)| l.depth == 1).unwrap();
        assert_eq!(parent.1.parent_run_id, Some(root_id));
        assert_eq!(
            tree.iter()
                .find(|(_, l)| l.depth == 2)
                .unwrap()
                .1
                .parent_run_id,
            Some(parent.0)
        );
    }
    root.close().await;
    middle.close().await;
    leaf.close().await;
}
#[tokio::test]
async fn inherited_depth_limit_prevents_descendant_dispatch() {
    let trace = Trace::default();
    let leaf = TestBuilder::default()
        .plugin(ChatModelPlugin::new(Echo))
        .plugin(EventPlugin::new("trace", trace.clone()))
        .build()
        .await
        .unwrap();
    let middle = TestBuilder::default()
        .plugin(AgentLoopPlugin::new(Delegator {
            child: leaf.clone(),
        }))
        .build()
        .await
        .unwrap();
    let root = TestBuilder::default()
        .plugin(AgentLoopPlugin::new(Delegator {
            child: middle.clone(),
        }))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("s", "x");
    request.limits.max_delegation_depth = 1;
    assert!(
        root.run(request)
            .await
            .unwrap_err()
            .to_string()
            .contains("depth limit")
    );
    assert!(trace.0.lock().unwrap().is_empty());
    root.close().await;
    middle.close().await;
    leaf.close().await;
}

struct PendingModel {
    entered: Arc<Notify>,
    dropped: Arc<AtomicUsize>,
}
struct OnDrop(Arc<AtomicUsize>);
impl Drop for OnDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
#[async_trait]
impl ChatModel for PendingModel {
    async fn generate(
        &self,
        _model_id: &str,
        _: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        let _guard = OnDrop(self.dropped.clone());
        self.entered.notify_one();
        std::future::pending().await
    }
}
#[tokio::test]
async fn parent_cancellation_reaches_child_provider_and_releases_its_future() {
    let entered = Arc::new(Notify::new());
    let dropped = Arc::new(AtomicUsize::new(0));
    let leaf = TestBuilder::default()
        .plugin(ChatModelPlugin::new(PendingModel {
            entered: entered.clone(),
            dropped: dropped.clone(),
        }))
        .build()
        .await
        .unwrap();
    let root = TestBuilder::default()
        .plugin(AgentLoopPlugin::new(Delegator {
            child: leaf.clone(),
        }))
        .build()
        .await
        .unwrap();
    let handle = root.start(RunRequest::new("s", "x")).unwrap();
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    root.cancel(&handle).unwrap();
    assert!(matches!(
        root.wait(handle).await,
        Err(AgentError::Cancelled)
    ));
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    tokio::time::timeout(Duration::from_secs(2), leaf.close())
        .await
        .unwrap();
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    root.close().await;
}

struct ConcurrentDelegator {
    child: Agent,
    entered: Arc<Notify>,
}
#[async_trait]
impl AgentLoop for ConcurrentDelegator {
    async fn run(
        &self,
        _: &RunRequest,
        context: &RunContext,
        _: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let mut request = RunRequest::new("one", "first");
        request.limits.max_total_tokens = 100;
        let first = self.child.run_child(request, context);
        tokio::pin!(first);
        tokio::select! { biased; _ = &mut first => panic!("child must remain pending"), _ = self.entered.notified() => {} }
        let mut request = RunRequest::new("two", "second");
        request.limits.max_total_tokens = 100;
        let error = self.child.run_child(request, context).await.unwrap_err();
        assert!(error.to_string().contains("parent budget"), "{error}");
        Ok(result(context, "capacity protected".into()))
    }
}
#[tokio::test]
async fn concurrent_children_cannot_spend_the_same_parent_allowance() {
    let entered = Arc::new(Notify::new());
    let dropped = Arc::new(AtomicUsize::new(0));
    let leaf = TestBuilder::default()
        .plugin(ChatModelPlugin::new(PendingModel {
            entered: entered.clone(),
            dropped,
        }))
        .build()
        .await
        .unwrap();
    let root = TestBuilder::default()
        .plugin(AgentLoopPlugin::new(ConcurrentDelegator {
            child: leaf.clone(),
            entered,
        }))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("s", "x");
    request.limits.max_total_tokens = 100;
    assert_eq!(
        root.run(request).await.unwrap().output,
        "capacity protected"
    );
    root.close().await;
    leaf.close().await;
}

struct AdmissionChecks {
    child: Agent,
    closed: Agent,
}
#[async_trait]
impl AgentLoop for AdmissionChecks {
    async fn run(
        &self,
        _: &RunRequest,
        context: &RunContext,
        _: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let mut invalid_options = RunRequest::new("invalid-options", "input");
        invalid_options.generation.temperature = Some(f64::NAN);
        for request in [RunRequest::new(" ", "input"), invalid_options] {
            assert!(matches!(
                self.child.run_child(request, context).await,
                Err(AgentError::InvalidConfiguration(_))
            ));
            assert_eq!(context.usage(), Usage::default());
        }
        assert!(matches!(
            self.closed
                .run_child(RunRequest::new("closed", "input"), context)
                .await,
            Err(AgentError::Closed)
        ));
        assert_eq!(context.usage(), Usage::default());
        assert!(matches!(
            self.child
                .run_child(RunRequest::new("busy", "input"), context)
                .await,
            Err(AgentError::SessionBusy(_))
        ));
        assert_eq!(context.usage(), Usage::default());
        let answer = self
            .child
            .run_child(RunRequest::new("valid", "input"), context)
            .await?;
        Ok(result(context, answer.output))
    }
}
struct GateEcho {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}
#[async_trait]
impl ChatModel for GateEcho {
    async fn generate(
        &self,
        _model_id: &str,
        request: ChatRequest,
        stream: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        if request.messages.last().unwrap().content.text() == "hold" {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Echo.generate(_model_id, request, stream).await
    }
}
#[tokio::test]
async fn rejected_children_preserve_budget_and_admission_for_a_valid_child() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let child = TestBuilder::default()
        .plugin(ChatModelPlugin::new(GateEcho {
            entered: entered.clone(),
            release: release.clone(),
        }))
        .build()
        .await
        .unwrap();
    let busy = child.start(RunRequest::new("busy", "hold")).unwrap();
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    let closed = TestBuilder::default()
        .plugin(ChatModelPlugin::new(Echo))
        .build()
        .await
        .unwrap();
    closed.close().await;
    let parent = TestBuilder::default()
        .plugin(AgentLoopPlugin::new(AdmissionChecks {
            child: child.clone(),
            closed,
        }))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("parent", "input");
    request.limits.max_child_runs = 1;
    request.limits.max_total_tokens = 500;
    let answer = parent.run(request).await.unwrap();
    assert_eq!(answer.output, "input");
    assert_eq!(answer.usage.input_tokens + answer.usage.output_tokens, 6);
    release.notify_one();
    child.wait(busy).await.unwrap();
    parent.close().await;
    child.close().await;
}

struct CapacityRetry {
    child: Agent,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}
#[async_trait]
impl AgentLoop for CapacityRetry {
    async fn run(
        &self,
        _: &RunRequest,
        context: &RunContext,
        _: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let mut request = RunRequest::new("first", "hold");
        request.limits.max_total_tokens = 500;
        let first = self.child.run_child(request, context);
        tokio::pin!(first);
        tokio::select! { biased; _ = &mut first => panic!("first child must wait"), _ = self.entered.notified() => {} }
        let error = self
            .child
            .run_child(RunRequest::new("no-capacity", "input"), context)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("parent budget"), "{error}");
        self.release.notify_one();
        first.await?;
        self.child
            .run_child(RunRequest::new("retry", "input"), context)
            .await?;
        Ok(result(context, "retry admitted".into()))
    }
}
#[tokio::test]
async fn budget_rejection_does_not_consume_a_child_slot() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let child = TestBuilder::default()
        .plugin(ChatModelPlugin::new(GateEcho {
            entered: entered.clone(),
            release: release.clone(),
        }))
        .build()
        .await
        .unwrap();
    let parent = TestBuilder::default()
        .plugin(AgentLoopPlugin::new(CapacityRetry {
            child: child.clone(),
            entered,
            release,
        }))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("parent", "input");
    request.limits.max_total_tokens = 500;
    request.limits.max_child_runs = 2;
    let answer = parent.run(request).await.unwrap();
    assert_eq!(answer.usage.input_tokens + answer.usage.output_tokens, 12);
    parent.close().await;
    child.close().await;
}
