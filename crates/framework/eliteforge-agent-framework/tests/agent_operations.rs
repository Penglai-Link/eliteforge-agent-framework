use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_test_support::TestBuilder;
use std::{
    any::TypeId,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

struct Immediate(Arc<AtomicUsize>);
#[async_trait]
impl AgentLoop for Immediate {
    async fn run(
        &self,
        _: &RunRequest,
        _: &RunContext,
        _: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(RunResult {
            output: "done".into(),
            content: "done".into(),
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

struct TestStream<const N: u8>(mpsc::UnboundedReceiver<EventEnvelope>);
struct Sender<const N: u8>(mpsc::UnboundedSender<EventEnvelope>);
#[derive(Clone)]
struct Presentation<const N: u8> {
    id: &'static str,
    wrong_type: bool,
}
impl<const N: u8> RunOutputFactory for Presentation<N> {
    fn output_type(&self) -> TypeId {
        TypeId::of::<TestStream<N>>()
    }
    fn prepare(
        &self,
        request: &RunRequest,
        _: RunId,
        extensions: &mut Extensions,
        outputs: &mut RunOutputs,
    ) -> Result<(), AgentError> {
        if request.session_id == "reject" {
            return Err(AgentError::InvalidConfiguration("rejected output".into()));
        }
        let (tx, rx) = mpsc::unbounded_channel();
        extensions.insert(Sender::<N>(tx));
        if self.wrong_type {
            outputs.insert(0u8)
        } else {
            outputs.insert(TestStream::<N>(rx))
        }
    }
}
#[async_trait]
impl<const N: u8> EventSink for Presentation<N> {
    fn on_published(&self, context: &RunContext, event: &EventEnvelope) {
        if let Some(sender) = context.extensions.get::<Sender<N>>() {
            let _ = sender.0.send(event.clone());
        }
    }
    async fn on_event(&self, _: &RunContext, _: &AgentEvent) {}
}
#[async_trait]
impl<const N: u8> Plugin for Presentation<N> {
    fn id(&self) -> &str {
        self.id
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        context.contribute(RunOutputService(Arc::new(self.clone())))?;
        context.contribute(EventService(Arc::new(self.clone())))
    }
}

#[tokio::test]
async fn multiple_configured_outputs_are_ready_before_start_and_remain_owned_by_the_agent() {
    let calls = Arc::new(AtomicUsize::new(0));
    let agent = TestBuilder::default()
        .plugin(AgentLoopPlugin::new(Immediate(calls.clone())))
        .plugin(Presentation::<1> {
            id: "one",
            wrong_type: false,
        })
        .plugin(Presentation::<2> {
            id: "two",
            wrong_type: false,
        })
        .build()
        .await
        .unwrap();
    let other = TestBuilder::default()
        .plugin(AgentLoopPlugin::new(Immediate(Arc::default())))
        .build()
        .await
        .unwrap();
    assert!(matches!(
        other.start_with_output::<TestStream<1>>(RunRequest::new("missing", "")),
        Err(AgentError::MissingService(_))
    ));
    let mut first = agent.start(RunRequest::new("one", "")).unwrap();
    let mut second = agent.start(RunRequest::new("two", "")).unwrap();
    assert!(other.output::<TestStream<1>>(&mut first).is_err());
    assert!(other.cancel(&first).is_err());
    assert!(other.control(&first).is_err());
    let mut first_output = agent.clone().output::<TestStream<1>>(&mut first).unwrap();
    let mut parallel_output = agent.output::<TestStream<2>>(&mut first).unwrap();
    let mut second_output = agent.output::<TestStream<1>>(&mut second).unwrap();
    assert!(agent.output::<TestStream<1>>(&mut first).is_err());
    for output in [&mut first_output.0, &mut parallel_output.0] {
        let start = output.try_recv().unwrap();
        assert!(matches!(start.event, AgentEvent::RunStarted { .. }));
        assert_eq!(start.run_id, first.run_id);
    }
    assert_eq!(second_output.0.try_recv().unwrap().run_id, second.run_id);
    assert!(agent.start(RunRequest::new("reject", "")).is_err());
    agent.wait(first).await.unwrap();
    agent.wait(second).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    agent.close().await;
    other.close().await;
}

#[tokio::test]
async fn duplicate_and_incorrect_output_registrations_fail_before_execution() {
    let calls = Arc::new(AtomicUsize::new(0));
    assert!(
        TestBuilder::default()
            .plugin(AgentLoopPlugin::new(Immediate(calls.clone())))
            .plugin(Presentation::<1> {
                id: "one",
                wrong_type: false
            })
            .plugin(Presentation::<1> {
                id: "duplicate",
                wrong_type: false
            })
            .build()
            .await
            .is_err()
    );
    let agent = TestBuilder::default()
        .plugin(AgentLoopPlugin::new(Immediate(calls.clone())))
        .plugin(Presentation::<1> {
            id: "wrong",
            wrong_type: true,
        })
        .build()
        .await
        .unwrap();
    assert!(
        agent
            .start_with_output::<TestStream<1>>(RunRequest::new("wrong", ""))
            .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    agent.close().await;
}

#[derive(Default)]
struct WorkState {
    entered: Notify,
    dropped: AtomicBool,
    closed: AtomicBool,
    cancellation: Mutex<Option<CancellationToken>>,
}
struct WorkPlugin(Arc<WorkState>);
#[async_trait]
impl Plugin for WorkPlugin {
    fn id(&self) -> &str {
        "test.operations"
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        context.provide(self.0.clone())?;
        let state = self.0.clone();
        context.on_shutdown(move || async move {
            if !state.dropped.load(Ordering::SeqCst) {
                return Err("closed before operation dropped".into());
            }
            state.closed.store(true, Ordering::SeqCst);
            Ok(())
        });
        Ok(())
    }
}
struct PendingWork;
struct WorkGuard(Arc<WorkState>);
impl Drop for WorkGuard {
    fn drop(&mut self) {
        self.0.dropped.store(true, Ordering::SeqCst);
    }
}
#[async_trait]
impl AgentOperation for PendingWork {
    type Service = Arc<WorkState>;
    type Output = ();
    async fn execute(
        self,
        state: Arc<Self::Service>,
        cancellation: CancellationToken,
    ) -> Result<(), AgentError> {
        let _guard = WorkGuard((*state).clone());
        *state.cancellation.lock().unwrap() = Some(cancellation);
        state.entered.notify_one();
        std::future::pending().await
    }
}

#[tokio::test]
async fn plugin_operations_are_cancelled_and_drained_before_shutdown() {
    for abort_caller in [false, true] {
        let state = Arc::new(WorkState::default());
        let agent = TestBuilder::default()
            .plugin(AgentLoopPlugin::new(Immediate(Arc::default())))
            .plugin(WorkPlugin(state.clone()))
            .build()
            .await
            .unwrap();
        let caller = agent.clone();
        let task = tokio::spawn(async move { caller.execute(PendingWork).await });
        tokio::time::timeout(Duration::from_secs(2), state.entered.notified())
            .await
            .unwrap();
        let task = if abort_caller {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            // The operation's children are cancelled even while Agent stays open.
            assert!(!agent.is_closed());
            assert!(
                state
                    .cancellation
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .is_cancelled()
            );
            None
        } else {
            Some(task)
        };
        let report = tokio::time::timeout(Duration::from_secs(2), agent.close())
            .await
            .unwrap();
        assert!(report.is_success(), "{report:?}");
        assert!(state.closed.load(Ordering::SeqCst));
        assert!(
            state
                .cancellation
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .is_cancelled()
        );
        if let Some(task) = task {
            assert!(matches!(task.await.unwrap(), Err(AgentError::Closed)));
        }
        assert!(matches!(
            agent.execute(PendingWork).await,
            Err(AgentError::Closed)
        ));
    }
}

#[tokio::test]
async fn missing_operation_service_is_an_error_and_does_not_block_close() {
    let agent = TestBuilder::default()
        .plugin(AgentLoopPlugin::new(Immediate(Arc::default())))
        .build()
        .await
        .unwrap();
    assert!(matches!(
        agent.execute(PendingWork).await,
        Err(AgentError::MissingService(_))
    ));
    assert!(
        tokio::time::timeout(Duration::from_secs(2), agent.close())
            .await
            .unwrap()
            .is_success()
    );
}

struct NumberPlugin;
#[async_trait]
impl Plugin for NumberPlugin {
    fn id(&self) -> &str {
        "number"
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        context.provide(42usize)
    }
}
struct NumberOperation(u8);
#[async_trait]
impl AgentOperation for NumberOperation {
    type Service = usize;
    type Output = usize;
    fn select(&self, mut services: Vec<Arc<usize>>) -> Result<Arc<usize>, AgentError> {
        assert_ne!(self.0, 1, "selection panic");
        Ok(services.remove(0))
    }
    async fn execute(self, service: Arc<usize>, _: CancellationToken) -> Result<usize, AgentError> {
        assert_ne!(self.0, 2, "execution panic");
        Ok(*service)
    }
}
#[tokio::test]
async fn plugin_panics_return_errors_and_release_operation_admission() {
    let agent = TestBuilder::default()
        .plugin(AgentLoopPlugin::new(Immediate(Arc::default())))
        .plugin(NumberPlugin)
        .build()
        .await
        .unwrap();
    for stage in [1, 2] {
        assert!(matches!(
            agent.execute(NumberOperation(stage)).await,
            Err(AgentError::Run(_))
        ));
    }
    assert_eq!(agent.execute(NumberOperation(0)).await.unwrap(), 42);
    assert!(
        tokio::time::timeout(Duration::from_secs(2), agent.close())
            .await
            .unwrap()
            .is_success()
    );
}
