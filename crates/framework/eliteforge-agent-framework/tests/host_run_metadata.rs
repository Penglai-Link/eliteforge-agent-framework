use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_storage_local::SqliteStateStore;
use eliteforge_agent_test_support::*;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Semaphore;

fn provenance(kind: &str, id: &str) -> MessageProvenance {
    MessageProvenance {
        sender: MessageSender {
            kind: kind.into(),
            id: Some(id.into()),
            display_name: Some(id.into()),
            avatar_url: None,
        },
        source: Some("task_reminder".into()),
        correlation: Some(json!({"taskCode":"task-1"})),
    }
}

struct Echo;
#[async_trait]
impl ChatModel for Echo {
    async fn generate(
        &self,
        _: &str,
        _: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        Ok(ChatResponse::text("done"))
    }
}

#[tokio::test]
async fn attribution_survives_sqlite_history_transcript_and_live_output_without_changing_role() {
    let directory = tempfile::tempdir().unwrap();
    let sqlite = Arc::new(
        SqliteStateStore::open(directory.path().join("history.sqlite"), Default::default())
            .await
            .unwrap(),
    );
    let agent = TestBuilder::default()
        .plugin(sqlite.clone().plugin())
        .plugin(ChatModelPlugin::new(Echo))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("owned-thread", "do work");
    request.host_context = Some(json!({"ownerDudeCode":"main","scope":{"workspaceCode":"a"}}));
    request.message_provenance = Some(provenance("dude", "main"));
    request.assistant_provenance = Some(provenance("agent", "worker"));
    let expected = request.host_metadata();
    let mut handle = agent.start(request).unwrap();
    let run_id = handle.run_id;
    let mut events = agent.events(&mut handle).unwrap();
    agent.wait(handle).await.unwrap();
    let store = agent
        .framework()
        .services()
        .unwrap()
        .get::<StateStoreService>()
        .unwrap()
        .0
        .clone();
    let checkpoint = store.load(run_id).await.unwrap();
    assert_eq!(checkpoint.host_metadata, expected);
    let history = agent
        .framework()
        .history()
        .unwrap()
        .read_session("owned-thread", HistoryQuery::default())
        .await
        .unwrap();
    assert_eq!(
        history
            .records
            .messages
            .iter()
            .find(|message| message.role == Role::User)
            .unwrap()
            .provenance,
        expected.message_provenance
    );
    assert_eq!(
        history
            .records
            .messages
            .iter()
            .find(|message| message.role == Role::Assistant)
            .unwrap()
            .provenance,
        expected.assistant_provenance
    );
    let input = checkpoint
        .history
        .messages
        .iter()
        .find(|m| m.role == Role::User)
        .unwrap();
    assert_eq!(input.provenance, expected.message_provenance);
    let output = checkpoint
        .history
        .messages
        .iter()
        .find(|m| m.role == Role::Assistant)
        .unwrap();
    assert_eq!(output.provenance, expected.assistant_provenance);
    assert_eq!(
        checkpoint
            .transcript
            .iter()
            .find(|m| m.role == Role::User)
            .unwrap()
            .provenance,
        expected.message_provenance
    );
    assert_eq!(
        checkpoint
            .transcript
            .iter()
            .find(|m| m.role == Role::Assistant)
            .unwrap()
            .provenance,
        expected.assistant_provenance
    );
    let mut observed = false;
    while let Ok(event) = events.try_recv() {
        if let AgentEvent::Output(output) = event.event
            && let OutputEvent::Custom { name, value } = *output
            && name == "framework.message.provenance"
        {
            assert_eq!(
                value["messageId"],
                json!(
                    checkpoint
                        .history
                        .messages
                        .iter()
                        .find(|m| m.role == Role::Assistant)
                        .unwrap()
                        .id
                )
            );
            assert_eq!(
                value["provenance"],
                serde_json::to_value(&expected.assistant_provenance).unwrap()
            );
            observed = true;
        }
    }
    assert!(observed);
    assert!(agent.framework().close().await.is_success());
    sqlite.close().await.unwrap();
}

struct Gate {
    entered: Arc<Semaphore>,
    release: Arc<Semaphore>,
}
#[async_trait]
impl ChatModel for Gate {
    async fn generate(
        &self,
        _: &str,
        _: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        self.entered.add_permits(1);
        self.release.acquire().await.unwrap().forget();
        Ok(ChatResponse::text("done"))
    }
}

#[tokio::test]
async fn independent_sessions_progress_and_observer_disconnect_never_cancels_host_owned_runs() {
    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(Gate {
            entered: entered.clone(),
            release: release.clone(),
        }))
        .build()
        .await
        .unwrap();
    let first = agent
        .start(RunRequest::new("interactive", "hello"))
        .unwrap();
    let observation = first.observe();
    let disconnected = observation.subscribe();
    let host = tokio::spawn({
        let agent = agent.clone();
        async move { agent.wait(first).await }
    });
    let second = agent.start(RunRequest::new("background", "work")).unwrap();
    let background = second.observe();
    let background_host = tokio::spawn({
        let agent = agent.clone();
        async move { agent.wait(second).await }
    });
    tokio::time::timeout(Duration::from_secs(5), entered.acquire_many(2))
        .await
        .unwrap()
        .unwrap()
        .forget();
    assert!(matches!(
        agent.start(RunRequest::new("interactive", "collision")),
        Err(AgentError::SessionBusy(_))
    ));
    drop(disconnected);
    let transient = observation.finished();
    tokio::pin!(transient);
    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut transient)
            .await
            .is_err()
    );
    assert!(!observation.is_finished());
    assert!(!background.is_finished());
    release.add_permits(2);
    tokio::time::timeout(Duration::from_secs(5), observation.finished())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), background.finished())
        .await
        .unwrap();
    assert_eq!(host.await.unwrap().unwrap().status, RunStatus::Completed);
    assert_eq!(
        background_host.await.unwrap().unwrap().status,
        RunStatus::Completed
    );
    let mut finished_events = observation.subscribe();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), finished_events.recv())
            .await
            .unwrap(),
        Err(tokio::sync::broadcast::error::RecvError::Closed)
    ));
    assert!(agent.framework().close().await.is_success());
}

struct Ask;
#[async_trait]
impl Tool for Ask {
    async fn call(&self, _: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("choose")
            .request_input(InteractionRequest::input("choose", json!({"type":"boolean"})).unwrap()))
    }
}
struct AskOnce;
#[async_trait]
impl ChatModel for AskOnce {
    async fn generate(
        &self,
        _: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        if request.messages.iter().any(|m| m.role == Role::Tool) {
            return Ok(ChatResponse::text("done"));
        }
        Ok(ChatResponse::tool_calls(vec![ToolCall::new(
            "ask",
            "ask",
            json!({}),
        )]))
    }
}
struct Capture(Arc<Mutex<Vec<RunHostMetadata>>>);
#[async_trait]
impl EventSink for Capture {
    async fn on_event(&self, context: &RunContext, event: &AgentEvent) {
        if matches!(event, AgentEvent::RunStarted { .. }) {
            self.0.lock().unwrap().push(context.host_metadata());
        }
    }
}
async fn recovery_agent(
    store: Arc<SqliteStateStore>,
    capture: Arc<Mutex<Vec<RunHostMetadata>>>,
) -> Agent {
    TestBuilder::default()
        .plugin(store.plugin())
        .plugin(ChatModelPlugin::new(AskOnce))
        .plugin(AgentLoopPlugin::new(
            WorkflowLoop::new(WorkflowDefinition {
                id: "restore".into(),
                version: 1,
                stages: vec![WorkflowStage::new("work")],
            })
            .unwrap(),
        ))
        .plugin(EventPlugin::new("capture", Capture(capture)))
        .plugin(ToolProviderPlugin::new(
            "tools",
            StaticToolProvider::new(vec![ToolBinding::new(
                ToolDescriptor::new("ask", "ask", "ask", json!({"type":"object"})),
                Ask,
            )]),
        ))
        .build()
        .await
        .unwrap()
}

#[tokio::test]
async fn fresh_host_restores_original_metadata_before_observers_and_tools_on_resume() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("restore.db");
    let store = Arc::new(
        SqliteStateStore::open(&path, Default::default())
            .await
            .unwrap(),
    );
    let capture = Arc::new(Mutex::new(Vec::new()));
    let agent = recovery_agent(store.clone(), capture.clone()).await;
    let mut request = RunRequest::new("work", "start");
    request.host_context = Some(json!({"owner":"dude","task":"task-1"}));
    request.message_provenance = Some(provenance("dude", "main"));
    request.assistant_provenance = Some(provenance("agent", "worker"));
    let expected = request.host_metadata();
    let handle = agent.start(request).unwrap();
    let run_id = handle.run_id;
    let result = agent.wait(handle).await.unwrap();
    assert_eq!(result.status, RunStatus::AwaitingInput);
    agent.framework().close().await;
    store.close().await.unwrap();
    let store = Arc::new(
        SqliteStateStore::open(&path, Default::default())
            .await
            .unwrap(),
    );
    let agent = recovery_agent(store.clone(), capture.clone()).await;
    let mut resume = RunRequest::new("work", "");
    resume.host_context = Some(json!({"owner":"wrong"}));
    resume.resume = Some(ResumeRequest {
        run_id,
        response: Some(InteractionResponse {
            interaction_id: result.interactions[0].id,
            value: json!(true),
        }),
    });
    assert_eq!(
        agent.run(resume).await.unwrap().status,
        RunStatus::Completed
    );
    assert_eq!(
        capture.lock().unwrap().as_slice(),
        &[expected.clone(), expected.clone()]
    );
    assert_eq!(store.load(run_id).await.unwrap().host_metadata, expected);
    agent.framework().close().await;
    store.close().await.unwrap();
}

#[tokio::test]
async fn fresh_host_keeps_original_owner_and_attributes_human_intervention_on_resume() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("restore.db");
    let store = Arc::new(
        SqliteStateStore::open(&path, Default::default())
            .await
            .unwrap(),
    );
    let capture = Arc::new(Mutex::new(Vec::new()));
    let agent = recovery_agent(store.clone(), capture.clone()).await;
    let mut request = RunRequest::new("work", "start");
    request.host_context = Some(json!({"owner":"dude","task":"task-1"}));
    request.message_provenance = Some(provenance("dude", "main"));
    request.assistant_provenance = Some(provenance("agent", "worker"));
    let expected = request.host_metadata();
    let handle = agent.start(request).unwrap();
    let run_id = handle.run_id;
    let result = agent.wait(handle).await.unwrap();
    assert_eq!(result.status, RunStatus::AwaitingInput);
    agent.framework().close().await;
    store.close().await.unwrap();
    let store = Arc::new(
        SqliteStateStore::open(&path, Default::default())
            .await
            .unwrap(),
    );
    let agent = recovery_agent(store.clone(), capture.clone()).await;
    // Hosts commonly commit with agent.answer before attaching the trusted
    // identity to the returned request. Recovery must fill attribution once.
    let mut resume = agent
        .answer("work", result.interactions[0].id, json!(true))
        .await
        .unwrap();
    assert!(
        InteractionStore::get(store.as_ref(), "work", result.interactions[0].id)
            .await
            .unwrap()
            .response_provenance
            .is_none()
    );
    resume.host_context = Some(json!({"owner":"wrong"}));
    let human = provenance("human", "operator");
    resume.message_provenance = Some(human.clone());
    resume.assistant_provenance = Some(provenance("agent", "wrong-worker"));
    resume.resume = Some(ResumeRequest {
        run_id,
        response: Some(InteractionResponse {
            interaction_id: result.interactions[0].id,
            value: json!(true),
        }),
    });
    assert_eq!(
        agent.run(resume).await.unwrap().status,
        RunStatus::Completed
    );
    let mut effective = expected.clone();
    effective.message_provenance = Some(human.clone());
    assert_eq!(
        capture.lock().unwrap().as_slice(),
        &[expected.clone(), effective]
    );
    let checkpoint = store.load(run_id).await.unwrap();
    assert_eq!(checkpoint.host_metadata, expected);
    assert_eq!(
        checkpoint.history.interactions[0].response_provenance,
        Some(human.clone())
    );
    assert_eq!(checkpoint.transcript.last().unwrap().role, Role::Assistant);
    assert!(
        checkpoint
            .transcript
            .iter()
            .any(|m| m.provenance == Some(human.clone()))
    );
    let history = agent
        .framework()
        .history()
        .unwrap()
        .read_session("work", HistoryQuery::default())
        .await
        .unwrap();
    assert_eq!(
        history.records.interactions[0].response_provenance,
        Some(human.clone())
    );
    assert_eq!(
        history
            .records
            .messages
            .iter()
            .find(|m| m.role == Role::User)
            .unwrap()
            .provenance,
        expected.message_provenance
    );
    let record = InteractionStore::get(store.as_ref(), "work", result.interactions[0].id)
        .await
        .unwrap();
    assert_eq!(record.response_provenance, Some(human.clone()));
    assert_eq!(record.resume_request().message_provenance, Some(human));
    assert!(
        InteractionStore::answer_with_provenance(
            store.as_ref(),
            "work",
            record.response.as_ref().unwrap(),
            Some(&provenance("dude", "forged"))
        )
        .await
        .is_err()
    );
    agent.framework().close().await;
    store.close().await.unwrap();
    let reopened = SqliteStateStore::open(&path, Default::default())
        .await
        .unwrap();
    let persisted = HistoryStore::read_session(&reopened, "work", HistoryQuery::default())
        .await
        .unwrap();
    assert_eq!(
        persisted.records.interactions[0].response_provenance,
        Some(provenance("human", "operator"))
    );
    reopened.close().await.unwrap();
}

struct IndependentThreadLoop {
    entered: Arc<Semaphore>,
    release: Arc<Semaphore>,
    inherited: Arc<Mutex<Vec<RunHostMetadata>>>,
}
#[async_trait]
impl AgentLoop for IndependentThreadLoop {
    async fn run(
        &self,
        request: &RunRequest,
        context: &RunContext,
        _: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        if request.input.text() == "child" {
            self.inherited.lock().unwrap().push(context.host_metadata());
            assert_eq!(request.host_context, context.host_context());
            self.entered.add_permits(1);
            self.release.acquire().await.unwrap().forget();
        } else {
            context
                .extensions
                .get::<AgentCollaborationService>()
                .unwrap()
                .0
                .create_thread(RunRequest::new("ignored", "child"), None, context)
                .await?;
        }
        Ok(RunResult {
            output: "done".into(),
            content: "done".into(),
            structured: None,
            interactions: vec![],
            status: RunStatus::Completed,
            usage: context.usage(),
            steps: 0,
            tool_calls: 0,
            external_session: None,
        })
    }
}
struct PublishedLifecycle(Arc<Mutex<Vec<&'static str>>>);
#[async_trait]
impl EventSink for PublishedLifecycle {
    async fn on_event(&self, _: &RunContext, _: &AgentEvent) {}
    fn on_published(&self, _: &RunContext, envelope: &EventEnvelope) {
        let kind = match &envelope.event {
            AgentEvent::RunFinished { .. } => "run_finished",
            AgentEvent::Collaboration(event)
                if matches!(event.as_ref(), CollaborationEvent::ThreadRunFinished { .. }) =>
            {
                "thread_finished"
            }
            _ => return,
        };
        self.0.lock().unwrap().push(kind);
    }
}
#[tokio::test]
async fn independent_thread_retains_host_lineage_and_finishes_after_origin_run() {
    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let inherited = Arc::new(Mutex::new(Vec::new()));
    let lifecycle = Arc::new(Mutex::new(Vec::new()));
    let agent = TestBuilder::default()
        .plugin(AgentLoopPlugin::new(IndependentThreadLoop {
            entered: entered.clone(),
            release: release.clone(),
            inherited: inherited.clone(),
        }))
        .plugin(EventPlugin::new(
            "published",
            PublishedLifecycle(lifecycle.clone()),
        ))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("origin", "parent");
    request.host_context =
        Some(json!({"ownerDudeCode":"main","taskCode":"task-1","scope":"platform"}));
    request.message_provenance = Some(provenance("user", "human"));
    request.assistant_provenance = Some(provenance("dude", "main"));
    let expected_context = request.host_context.clone();
    let expected_sender = request.assistant_provenance.clone();
    assert_eq!(
        agent.run(request).await.unwrap().status,
        RunStatus::Completed
    );
    tokio::time::timeout(Duration::from_secs(5), entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    assert!(!lifecycle.lock().unwrap().contains(&"thread_finished"));
    assert_eq!(inherited.lock().unwrap()[0].host_context, expected_context);
    assert_eq!(
        inherited.lock().unwrap()[0].message_provenance,
        expected_sender
    );
    release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if lifecycle.lock().unwrap().contains(&"thread_finished") {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(lifecycle.lock().unwrap()[0], "run_finished");
    assert_eq!(lifecycle.lock().unwrap().last(), Some(&"thread_finished"));
    assert!(agent.framework().close().await.is_success());
}

struct PendingRecoveryStore(Arc<Semaphore>);
#[async_trait]
impl StateStore for PendingRecoveryStore {
    async fn create(&self, _: Checkpoint, _: Option<RunId>) -> Result<Checkpoint, StateError> {
        unreachable!()
    }
    async fn load(&self, _: RunId) -> Result<Checkpoint, StateError> {
        self.0.add_permits(1);
        std::future::pending().await
    }
    async fn latest(&self, _: &str) -> Result<Option<Checkpoint>, StateError> {
        unreachable!()
    }
    async fn latest_scope(&self, _: &str, _: &str) -> Result<Option<Checkpoint>, StateError> {
        unreachable!()
    }
    async fn claim(&self, _: RunId, _: RunId, _: Duration) -> Result<ExecutionLease, StateError> {
        unreachable!()
    }
    async fn renew(&self, _: &ExecutionLease, _: Duration) -> Result<ExecutionLease, StateError> {
        unreachable!()
    }
    async fn commit(
        &self,
        _: &ExecutionLease,
        _: u64,
        _: Checkpoint,
    ) -> Result<Checkpoint, StateError> {
        unreachable!()
    }
    async fn release(&self, _: &ExecutionLease) -> Result<(), StateError> {
        unreachable!()
    }
}
#[tokio::test]
async fn pending_checkpoint_metadata_recovery_obeys_cancellation_and_deadline() {
    for cancel in [true, false] {
        let entered = Arc::new(Semaphore::new(0));
        let agent = TestBuilder::default()
            .plugin(StateStorePlugin::new(PendingRecoveryStore(entered.clone())))
            .plugin(ChatModelPlugin::new(Echo))
            .build()
            .await
            .unwrap();
        let mut request = RunRequest::new("recover", "");
        request.resume = Some(ResumeRequest {
            run_id: RunId::new_v4(),
            response: None,
        });
        if !cancel {
            request.limits.run_timeout = Duration::from_millis(40);
        }
        let handle = agent.start(request).unwrap();
        tokio::time::timeout(Duration::from_secs(5), entered.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        if cancel {
            agent.cancel(&handle).unwrap();
        }
        let error = tokio::time::timeout(Duration::from_secs(5), agent.wait(handle))
            .await
            .unwrap()
            .unwrap_err();
        if cancel {
            assert!(matches!(error.primary(), AgentError::Cancelled));
        } else {
            assert!(error.to_string().contains("deadline"));
        }
        assert!(agent.framework().close().await.is_success());
    }
}
