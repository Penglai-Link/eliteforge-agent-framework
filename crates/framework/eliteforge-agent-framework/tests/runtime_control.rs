//! Lifecycle and concurrency contracts exercised through the public host API.

use eliteforge_agent_test_support::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use async_trait::async_trait;
use eliteforge_agent_framework::*;
use serde_json::{Value, json};
use tokio::sync::{Barrier, Notify};
use tokio::time::timeout;

const DEADLINE: Duration = Duration::from_secs(5);

#[derive(Clone, Default)]
struct GatedModel {
    entered: Arc<Notify>,
    release: Arc<Notify>,
    calls: Arc<AtomicUsize>,
    observed_messages: Arc<Mutex<Vec<Message>>>,
}

#[async_trait]
impl ChatModel for GatedModel {
    async fn generate(
        &self,
        _model_id: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.entered.notify_one();
            self.release.notified().await;
            return Ok(ChatResponse::text("first draft"));
        }
        *self.observed_messages.lock().unwrap() = request.messages.clone();
        let user_input = request
            .messages
            .iter()
            .filter(|message| message.role == Role::User)
            .map(|message| message.content.text())
            .collect::<Vec<_>>()
            .join(" | ");
        Ok(ChatResponse::text(user_input))
    }
}

#[tokio::test]
async fn host_notification_reaches_model_with_system_provenance() {
    let model = GatedModel::default();
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        .build().await.unwrap();
    let handle = agent.start(RunRequest::new("session", "original task")).unwrap();
    let control = agent.control(&handle).unwrap();
    let waiting = tokio::spawn({
        let agent = agent.clone();
        async move { agent.wait(handle).await }
    });
    timeout(DEADLINE, model.entered.notified()).await.unwrap();
    control.notify("child finished", MessageProvenance {
        sender: MessageSender { kind: "SYSTEM".into(), display_name: Some("System notice".into()), ..Default::default() },
        source: Some("ChildRunCompletion".into()),
        correlation: None,
    }).unwrap();
    model.release.notify_one();
    let result = timeout(DEADLINE, waiting).await.unwrap().unwrap().unwrap();
    assert!(result.output.contains("child finished"));
    let messages = model.observed_messages.lock().unwrap();
    let notification = messages.iter().find(|message| message.content.text() == "child finished").unwrap();
    assert_eq!(notification.provenance.as_ref().unwrap().sender.kind, "SYSTEM");
    assert_eq!(notification.provenance.as_ref().unwrap().source.as_deref(), Some("ChildRunCompletion"));
}

struct RunResources(Arc<AtomicUsize>);

#[async_trait]
impl ToolProvider for RunResources {
    async fn lease(&self, _: &RunContext) -> Result<ToolLease, AgentError> {
        let released = self.0.clone();
        Ok(ToolLease::new(vec![]).with_cleanup(move || {
            released.fetch_add(1, Ordering::SeqCst);
        }))
    }
}

struct ContainerResources(Arc<AtomicUsize>);

#[async_trait]
impl Plugin for ContainerResources {
    fn id(&self) -> &str {
        "test.container-resources"
    }

    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let released = self.0.clone();
        context.on_cleanup(move || {
            released.fetch_add(1, Ordering::SeqCst);
        });
        Ok(())
    }
}

#[tokio::test]
async fn cancellation_releases_run_resources_and_allows_reusing_the_session() {
    let model = GatedModel::default();
    let released = Arc::new(AtomicUsize::new(0));
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        .plugin(ToolProviderPlugin::new(
            "test.resources",
            RunResources(released.clone()),
        ))
        .build()
        .await
        .unwrap();
    let mut handle = agent
        .start(RunRequest::new("session", "first input"))
        .unwrap();
    let mut events = agent.events(&mut handle).unwrap();
    let control = agent.control(&handle).unwrap();
    let waiting = tokio::spawn({
        let agent = agent.clone();
        async move { agent.wait(handle).await }
    });
    timeout(DEADLINE, model.entered.notified()).await.unwrap();
    assert!(matches!(
        agent.start(RunRequest::new("session", "overlapping input")),
        Err(AgentError::SessionBusy(_))
    ));

    agent.cancel(&control).unwrap();
    assert!(agent.steer(&control, "too late").is_err());
    assert!(matches!(
        timeout(DEADLINE, waiting).await.unwrap().unwrap(),
        Err(AgentError::Cancelled)
    ));
    assert_eq!(released.load(Ordering::SeqCst), 1);
    let mut terminal_count = 0;
    while let Ok(event) = events.try_recv() {
        assert_eq!(event.session_id, "session");
        if matches!(event.event, AgentEvent::RunCancelled) {
            terminal_count += 1;
        }
    }
    assert_eq!(terminal_count, 1);

    let result = timeout(DEADLINE, agent.run(RunRequest::new("session", "new input")))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, RunStatus::Completed);
    assert!(result.output.contains("new input"));
    assert_eq!(released.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn closing_framework_cancels_active_runs_cleans_plugins_and_rejects_new_runs() {
    let model = GatedModel::default();
    let cleaned = Arc::new(AtomicUsize::new(0));
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        .plugin(ContainerResources(cleaned.clone()))
        .build()
        .await
        .unwrap();
    let handle = agent.start(RunRequest::new("session", "run")).unwrap();
    timeout(DEADLINE, model.entered.notified()).await.unwrap();

    timeout(DEADLINE, agent.framework().close()).await.unwrap();

    assert!(agent.is_closed());
    assert_eq!(cleaned.load(Ordering::SeqCst), 1);
    assert!(matches!(
        agent.wait(handle).await,
        Err(AgentError::Cancelled)
    ));
    assert!(matches!(
        agent.start(RunRequest::new("other", "new run")),
        Err(AgentError::Closed)
    ));
    agent.framework().close().await;
    assert_eq!(cleaned.load(Ordering::SeqCst), 1);
}

struct GatedCleanup {
    entered: Arc<Notify>,
    release: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    cleaned: Arc<AtomicUsize>,
}

#[async_trait]
impl Plugin for GatedCleanup {
    fn id(&self) -> &str {
        "test.gated-cleanup"
    }

    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let entered = self.entered.clone();
        let release = self.release.lock().unwrap().take().unwrap();
        let cleaned = self.cleaned.clone();
        context.on_cleanup(move || {
            entered.notify_one();
            // Deliberately gate a synchronous callback to exercise two callers.
            // The deadline also ensures a failing test cannot strand a worker.
            release.recv_timeout(DEADLINE).unwrap();
            cleaned.fetch_add(1, Ordering::SeqCst);
        });
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_close_callers_wait_for_the_same_plugin_cleanup() {
    let entered = Arc::new(Notify::new());
    let cleaned = Arc::new(AtomicUsize::new(0));
    let (release, receiver) = std::sync::mpsc::channel();
    let agent = Arc::new(
        TestBuilder::default()
            .plugin(ChatModelPlugin::new(GatedModel::default()))
            .plugin(GatedCleanup {
                entered: entered.clone(),
                release: std::sync::Mutex::new(Some(receiver)),
                cleaned: cleaned.clone(),
            })
            .build()
            .await
            .unwrap(),
    );
    let first_agent = agent.clone();
    let first_close = tokio::spawn(async move { first_agent.framework().close().await });
    timeout(DEADLINE, entered.notified()).await.unwrap();

    let framework = agent.framework();
    let mut second_close = Box::pin(framework.close());
    assert!(futures::poll!(second_close.as_mut()).is_pending());
    assert_eq!(cleaned.load(Ordering::SeqCst), 0);
    release.send(()).unwrap();

    timeout(DEADLINE, second_close).await.unwrap();
    timeout(DEADLINE, first_close).await.unwrap().unwrap();
    assert_eq!(cleaned.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn steering_during_a_final_model_response_is_consumed_without_changing_its_role() {
    let model = GatedModel::default();
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        .build()
        .await
        .unwrap();
    let mut handle = agent
        .start(RunRequest::new("session", "original task"))
        .unwrap();
    let mut events = agent.events(&mut handle).unwrap();
    let control = agent.control(&handle).unwrap();
    assert_eq!(control.run_id(), handle.run_id);
    let other_control = control.clone();
    let waiting = tokio::spawn({
        let agent = agent.clone();
        async move { agent.wait(handle).await }
    });
    timeout(DEADLINE, model.entered.notified()).await.unwrap();
    agent.steer(&control, "include tests").unwrap();
    agent.steer(&other_control, "explain the result").unwrap();
    drop(other_control);
    model.release.notify_one();

    let result = timeout(DEADLINE, waiting).await.unwrap().unwrap().unwrap();

    assert_eq!(result.status, RunStatus::Completed);
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    assert!(result.output.contains("include tests"));
    assert!(result.output.contains("explain the result"));
    assert!(agent.steer(&control, "after completion").is_err());

    // Keeping a control alive must not keep the event stream open.
    let steered = timeout(DEADLINE, async {
        let mut steered = Vec::new();
        loop {
            match events.recv().await {
                Ok(envelope) => {
                    if let AgentEvent::Steered(text) = envelope.event {
                        steered.push(text);
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return steered,
                Err(error) => panic!("unexpected event loss: {error}"),
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(steered, ["include tests", "explain the result"]);
}

#[tokio::test]
async fn dropping_all_external_controls_does_not_cancel_the_waiting_run() {
    let model = GatedModel::default();
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        .build()
        .await
        .unwrap();
    let handle = agent.start(RunRequest::new("session", "task")).unwrap();
    let control = agent.control(&handle).unwrap();
    let other_control = control.clone();
    let waiting = tokio::spawn({
        let agent = agent.clone();
        async move { agent.wait(handle).await }
    });
    timeout(DEADLINE, model.entered.notified()).await.unwrap();
    drop(control);
    drop(other_control);
    model.release.notify_one();
    let result = timeout(DEADLINE, waiting).await.unwrap().unwrap().unwrap();
    assert_eq!(result.status, RunStatus::Completed);
    assert_eq!(result.output, "first draft");
}

#[tokio::test]
async fn retained_controls_do_not_prevent_owner_drop_from_cancelling_the_run() {
    for drop_wait_future in [false, true] {
        let model = GatedModel::default();
        let released = Arc::new(AtomicUsize::new(0));
        let agent = TestBuilder::default()
            .plugin(ChatModelPlugin::new(model.clone()))
            .plugin(ToolProviderPlugin::new(
                "resources",
                RunResources(released.clone()),
            ))
            .build()
            .await
            .unwrap();
        let mut handle = agent.start(RunRequest::new("session", "task")).unwrap();
        let control = agent.control(&handle).unwrap();
        let mut events = agent.events(&mut handle).unwrap();
        timeout(DEADLINE, model.entered.notified()).await.unwrap();
        if drop_wait_future {
            let mut waiting = Box::pin(agent.wait(handle));
            assert!(futures::poll!(waiting.as_mut()).is_pending());
            drop(waiting);
        } else {
            drop(handle);
        }
        assert!(agent.steer(&control, "owner gone").is_err());
        let cancelled = timeout(DEADLINE, async {
            let mut cancelled = 0;
            loop {
                match events.recv().await {
                    Ok(envelope) => {
                        if matches!(envelope.event, AgentEvent::RunCancelled) {
                            cancelled += 1;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return cancelled,
                    Err(error) => panic!("unexpected event loss: {error}"),
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(cancelled, 1);
        assert_eq!(released.load(Ordering::SeqCst), 1);

        // A retained control only targets the old execution, even when the
        // same session is reused. Its cancellation must not reach the new run.
        let next = agent.start(RunRequest::new("session", "new task")).unwrap();
        assert_ne!(next.run_id, control.run_id());
        agent.cancel(&control).unwrap();
        assert_eq!(agent.wait(next).await.unwrap().status, RunStatus::Completed);
        agent.framework().close().await;
    }
}

struct BatchModel {
    calls: AtomicUsize,
}

#[async_trait]
impl ChatModel for BatchModel {
    async fn generate(
        &self,
        _model_id: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok(ChatResponse::tool_calls(
                (0..4)
                    .map(|i| ToolCall::new(format!("call-{i}"), "work", json!({"i": i})))
                    .collect(),
            ))
        } else {
            let ids: Vec<_> = request
                .messages
                .iter()
                .filter(|message| message.role == Role::Tool)
                .map(|message| message.tool_call_id.as_deref().unwrap())
                .collect();
            assert_eq!(ids, ["call-0", "call-1", "call-2", "call-3"]);
            Ok(ChatResponse::text("done"))
        }
    }
}

struct ParallelTool {
    barrier: Arc<Barrier>,
    active: Arc<AtomicUsize>,
    maximum: Arc<AtomicUsize>,
}

struct ActiveTool(Arc<AtomicUsize>);

impl Drop for ActiveTool {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl Tool for ParallelTool {
    async fn call(&self, arguments: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        let _guard = ActiveTool(self.active.clone());
        self.maximum.fetch_max(active, Ordering::SeqCst);
        // Four calls must execute as two concurrent pairs. Sequential execution
        // deadlocks this barrier; unbounded execution is detected by maximum.
        self.barrier.wait().await;
        Ok(ToolOutput::text(arguments.to_string()))
    }
}

#[tokio::test]
async fn parallel_tools_obey_the_cap_and_keep_model_call_order_in_history() {
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(BatchModel {
            calls: AtomicUsize::new(0),
        }))
        .plugin(ToolProviderPlugin::new(
            "test.parallel",
            StaticToolProvider::new(vec![
                ToolBinding::new(
                    ToolDescriptor::new(
                        "work.run",
                        "work",
                        "Do independent work",
                        json!({"type":"object"}),
                    ),
                    ParallelTool {
                        barrier: Arc::new(Barrier::new(2)),
                        active: active.clone(),
                        maximum: maximum.clone(),
                    },
                )
                .parallel_safe(),
            ]),
        ))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("session", "work");
    request.limits.max_parallel_tools = 2;

    let result = timeout(DEADLINE, agent.run(request))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(result.status, RunStatus::Completed);
    assert_eq!(result.tool_calls, 4);
    assert_eq!(maximum.load(Ordering::SeqCst), 2);
    assert_eq!(active.load(Ordering::SeqCst), 0);
}
