use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_agui::{
    AgUiOptions, AgUiPlugin, AgUiStream, Event, EventType, ag_ui,
};
use eliteforge_agent_official_plugin_storage_local::{SqliteOptions, SqliteStateStore};
use eliteforge_agent_test_support::*;
use futures::StreamExt;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Barrier, Notify};

async fn collect(mut stream: AgUiStream) -> Vec<Event> {
    let mut events = Vec::new();
    while let Some(event) = stream.recv().await {
        events.push(event);
    }
    events
}
async fn complete(
    agent: Agent,
    handle: RunHandle,
    stream: AgUiStream,
) -> (Result<RunResult, AgentError>, Vec<Event>) {
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(agent.wait(handle), collect(stream))
    })
    .await
    .unwrap()
}
async fn validate_client(events: &[Event]) {
    let Event::RunStarted(start) = &events[0] else {
        panic!("missing RUN_STARTED")
    };
    let transport = ag_ui::client::transport::ReplayTransport::new(events.to_vec());
    let mut session = ag_ui::client::Session::<_>::new(transport, start.thread_id.clone());
    let mut run = session.send("task");
    let mut done = false;
    while let Some(update) = run.next().await {
        match update {
            ag_ui::client::Update::Error(error) => panic!("SDK rejected stream: {error}"),
            ag_ui::client::Update::Done(_) => done = true,
            _ => {}
        }
    }
    assert!(done);
}
fn text(events: &[Event]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            Event::TextMessageContent(value) => Some(value.delta.as_str()),
            _ => None,
        })
        .collect()
}
fn types(events: &[Event], ty: EventType) -> usize {
    events.iter().filter(|e| e.event_type() == ty).count()
}
fn code(events: &[Event]) -> Option<&str> {
    match events.last() {
        Some(Event::RunError(error)) => error.code.as_deref(),
        _ => None,
    }
}

struct Streaming;

struct ControlledExternal {
    prefix: &'static str,
    final_text: &'static str,
    release: Arc<Notify>,
    fail: bool,
}

#[async_trait]
impl ExternalAgent for ControlledExternal {
    async fn run(
        &self,
        _: ExternalAgentRequest,
        sink: &dyn ExternalAgentEventSink,
    ) -> Result<ExternalAgentResult, AgentError> {
        sink.emit(ExternalAgentEvent::ReasoningDelta("thinking".into()))
            .await;
        if !self.prefix.is_empty() {
            sink.emit(ExternalAgentEvent::TextDelta(self.prefix.into()))
                .await;
        }
        sink.emit(ExternalAgentEvent::Status("running".into()))
            .await;
        sink.emit(ExternalAgentEvent::Tool {
            name: "external-tool".into(),
            detail: json!({}),
        })
        .await;
        sink.emit(ExternalAgentEvent::Approval {
            detail: json!({"pending":true}),
        })
        .await;
        self.release.notified().await;
        if self.fail {
            return Err(AgentError::Run("external failure".into()));
        }
        Ok(ExternalAgentResult {
            content: self.final_text.into(),
            structured: None,
            session: Some(ExternalSessionRef {
                session_id: "external-session".into(),
                run_id: None,
            }),
            usage: Usage::default(),
        })
    }
}

async fn external_agent(
    prefix: &'static str,
    final_text: &'static str,
    fail: bool,
) -> (Agent, Arc<Notify>) {
    let release = Arc::new(Notify::new());
    let agent = TestBuilder::new()
        .default_models(false)
        .plugin(AgentLoopPlugin::new(ExternalAgentLoop))
        .plugin(ExternalAgentPlugin::new(ControlledExternal {
            prefix,
            final_text,
            fail,
            release: release.clone(),
        }))
        .plugin(
            AgUiPlugin::new(AgUiOptions {
                include_reasoning: true,
                ..Default::default()
            })
            .unwrap(),
        )
        .build()
        .await
        .unwrap();
    (agent, release)
}

#[tokio::test]
async fn external_deltas_and_observations_arrive_before_completion() {
    let (agent, release) = external_agent("hello", "hello world", false).await;
    let (handle, mut stream) = agent
        .start_with_output::<AgUiStream>(RunRequest::new("external", "task"))
        .unwrap();
    let mut events = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = stream.recv().await.expect("stream ended before external completion");
            let observed = matches!(&event, Event::Custom(custom) if custom.name == "framework.external_agent");
            events.push(event);
            if observed { break; }
        }
    }).await.unwrap();
    assert_eq!(text(&events), "hello");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::ReasoningMessageContent(_)))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::RunFinished(_)))
    );
    release.notify_one();
    let (result, tail) = complete(agent, handle, stream).await;
    assert_eq!(result.unwrap().output, "hello world");
    events.extend(tail);
    assert_eq!(text(&events), "hello world");
    assert_eq!(types(&events, EventType::TextMessageStart), 1);
    assert_eq!(events.iter().filter(|event| matches!(event, Event::Custom(custom) if custom.name == "framework.external_agent")).count(), 4);
    assert_eq!(types(&events, EventType::ToolCallStart), 0);
    validate_client(&events).await;
}

#[tokio::test]
async fn external_final_text_handles_identical_different_and_no_prefix() {
    for (prefix, final_text, expected, messages) in [
        ("answer", "answer", "answer", 1),
        ("working", "answer", "workinganswer", 2),
        ("", "answer", "answer", 1),
    ] {
        let (agent, release) = external_agent(prefix, final_text, false).await;
        release.notify_one();
        let (handle, stream) = agent
            .start_with_output::<AgUiStream>(RunRequest::new("external", "task"))
            .unwrap();
        let (result, events) = complete(agent, handle, stream).await;
        assert_eq!(result.unwrap().output, final_text);
        assert_eq!(text(&events), expected);
        assert_eq!(types(&events, EventType::TextMessageStart), messages);
        assert_eq!(types(&events, EventType::TextMessageEnd), messages);
        validate_client(&events).await;
    }
}

#[tokio::test]
async fn external_failure_cancel_and_timeout_close_streamed_messages() {
    for mode in ["failure", "cancel", "timeout"] {
        let (agent, release) = external_agent("partial", "answer", mode == "failure").await;
        let mut request = RunRequest::new("external", "task");
        if mode == "timeout" {
            request.limits.run_timeout = Duration::from_millis(100);
        }
        let (handle, mut stream) = agent.start_with_output::<AgUiStream>(request).unwrap();
        let mut events = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let event = stream.recv().await.unwrap();
                let text = matches!(event, Event::TextMessageContent(_));
                events.push(event);
                if text {
                    break;
                }
            }
        })
        .await
        .unwrap();
        match mode {
            "failure" => release.notify_one(),
            "cancel" => agent.cancel(&handle).unwrap(),
            _ => {}
        }
        let (result, tail) = complete(agent, handle, stream).await;
        assert!(result.is_err());
        events.extend(tail);
        assert_eq!(
            types(&events, EventType::TextMessageEnd),
            1,
            "{mode}: {events:?}"
        );
        assert_eq!(types(&events, EventType::RunError), 1, "{mode}: {events:?}");
    }
}

#[async_trait]
impl ChatModel for Streaming {
    async fn generate(
        &self,
        _model_id: &str,
        _: ChatRequest,
        stream: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        stream.reasoning_delta("可公开的推理摘要").await;
        stream.text_delta("你").await;
        stream.text_delta("好\n").await;
        // Providers may return a final suffix that was not streamed.
        Ok(ChatResponse::text("你好\n世界"))
    }
}
#[tokio::test]
async fn streaming_suffix_reasoning_sse_and_multiple_runs_remain_isolated() {
    let plugin = AgUiPlugin::default();
    let agent = TestBuilder::default()
        .plugin(plugin.clone())
        .plugin(ChatModelPlugin::new(Streaming))
        .build()
        .await
        .unwrap();
    let (a, a_stream) = agent
        .start_with_output::<AgUiStream>(RunRequest::new("a", "task"))
        .unwrap();
    let (b, b_stream) = agent
        .start_with_output::<AgUiStream>(RunRequest::new("b", "task"))
        .unwrap();
    let a_id = a.run_id;
    let ((a_result, a_events), (b_result, b_events)) = tokio::join!(
        complete(agent.clone(), a, a_stream),
        complete(agent.clone(), b, b_stream)
    );
    assert_eq!(a_result.unwrap().output, "你好\n世界");
    assert_eq!(b_result.unwrap().output, "你好\n世界");
    for events in [&a_events, &b_events] {
        validate_client(events).await;
        assert_eq!(text(events), "你好\n世界");
        assert_eq!(types(events, EventType::TextMessageStart), 1);
        assert_eq!(types(events, EventType::ReasoningMessageStart), 1);
        assert_eq!(types(events, EventType::ReasoningMessageEnd), 1);
        assert_eq!(types(events, EventType::RunFinished), 1);
        for event in events {
            let sse = ag_ui::SseFormatter::new().encode_to_string(event).unwrap();
            assert!(sse.starts_with("data: {") && sse.ends_with("\n\n"));
            assert_eq!(sse.lines().count(), 2);
            assert_eq!(
                serde_json::from_str::<Event>(sse.strip_prefix("data: ").unwrap().trim_end())
                    .unwrap(),
                *event
            );
        }
    }
    assert!(
        a_events
            .iter()
            .all(|e| e.base().metadata.as_ref().unwrap()["framework"]["runId"] == a_id.to_string())
    );
    let messages = |events: &[Event]| {
        events
            .iter()
            .filter_map(|event| match event {
                Event::TextMessageStart(e) => Some(e.message_id.to_string()),
                _ => None,
            })
            .collect::<BTreeSet<_>>()
    };
    assert!(messages(&a_events).is_disjoint(&messages(&b_events)));
    agent.framework().close().await;
}

struct ToolModel {
    barrier: Option<Arc<Barrier>>,
}
#[async_trait]
impl ChatModel for ToolModel {
    async fn generate(
        &self,
        _model_id: &str,
        request: ChatRequest,
        stream: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        if request.messages.iter().any(|m| m.role == Role::Tool) {
            stream.text_delta("done").await;
            return Ok(ChatResponse::text("done"));
        }
        if let Some(barrier) = &self.barrier {
            barrier.wait().await;
        }
        stream.reasoning_delta("checking").await;
        Ok(ChatResponse::tool_calls(vec![ToolCall::new(
            "same-provider-id",
            "work",
            json!({"value":"input"}),
        )]))
    }
}
struct Work {
    count: Arc<AtomicUsize>,
    large: bool,
}
#[async_trait]
impl Tool for Work {
    async fn call(&self, _: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        self.count.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput {
            structured: Some(
                json!({"data":if self.large { "x".repeat(8192) } else { "small".into() }}),
            ),
            ..ToolOutput::text("result")
        })
    }
}
fn tools(count: Arc<AtomicUsize>, large: bool) -> ToolProviderPlugin {
    ToolProviderPlugin::new("test.tools", StaticToolProvider::new(vec![ToolBinding::new(
        ToolDescriptor::new("work", "work", "Work", json!({"type":"object","properties":{"value":{"type":"string"}},"required":["value"]})),
        Work { count, large }).parallel_safe()]))
}
#[tokio::test]
async fn offloaded_tool_results_preserve_reference_and_tool_lifecycle() {
    use eliteforge_agent_context::ToolOutputOffloader;
    use eliteforge_agent_official_plugin_storage_local::{FileArtifactLimits, FileArtifactStore};
    let dir = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let store =
        Arc::new(FileArtifactStore::open(dir.path(), FileArtifactLimits::default()).unwrap());
    let plugin = AgUiPlugin::default();
    let count = Arc::new(AtomicUsize::new(0));
    let agent = TestBuilder::default()
        .plugin(plugin.clone())
        .plugin(ChatModelPlugin::new(ToolModel { barrier: None }))
        .plugin(tools(count.clone(), true))
        .plugin(ToolMiddlewarePlugin::new(
            "offload",
            ToolOutputOffloader::new(store, 1024, 32).unwrap(),
        ))
        .build()
        .await
        .unwrap();
    let (handle, stream) = agent
        .start_with_output::<AgUiStream>(RunRequest::new("tools", "task"))
        .unwrap();
    let (result, events) = complete(agent.clone(), handle, stream).await;
    assert_eq!(result.unwrap().tool_calls, 1);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    validate_client(&events).await;
    let start = events
        .iter()
        .position(|e| e.event_type() == EventType::ToolCallStart)
        .unwrap();
    let args = events
        .iter()
        .position(|e| e.event_type() == EventType::ToolCallArgs)
        .unwrap();
    let end = events
        .iter()
        .position(|e| e.event_type() == EventType::ToolCallEnd)
        .unwrap();
    let result = events
        .iter()
        .position(|e| e.event_type() == EventType::ToolCallResult)
        .unwrap();
    assert!(start < args && args < end && end < result);
    let Event::ToolCallResult(result) = &events[result] else {
        unreachable!()
    };
    let payload: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(payload["isError"], false);
    assert!(payload["structured"].is_null());
    assert!(
        payload["resultReference"]["uri"]
            .as_str()
            .unwrap()
            .starts_with("artifact:")
    );
    assert!(!result.content.contains(&"x".repeat(2048)));
    agent.framework().close().await;
}

#[tokio::test]
async fn parallel_dag_nodes_with_identical_provider_call_ids_have_distinct_messages_and_results() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        SqliteStateStore::open(dir.path().join("dag.db"), SqliteOptions::default())
            .await
            .unwrap(),
    );
    let plugin = AgUiPlugin::default();
    let graph = DagWorkflowDefinition {
        id: "dag".into(),
        version: 1,
        nodes: vec![
            DagNode::react(WorkflowStage::new("a")),
            DagNode::react(WorkflowStage::new("b")),
        ],
        edges: vec![],
        max_parallel_nodes: 2,
    };
    let agent = TestBuilder::default()
        .plugin(plugin.clone())
        .plugin(store.clone().plugin())
        .plugin(AgentLoopPlugin::new(DagWorkflowLoop::new(graph).unwrap()))
        .plugin(ChatModelPlugin::new(ToolModel {
            barrier: Some(Arc::new(Barrier::new(2))),
        }))
        .plugin(tools(Arc::new(AtomicUsize::new(0)), false))
        .build()
        .await
        .unwrap();
    let (handle, stream) = agent
        .start_with_output::<AgUiStream>(RunRequest::new("dag", "task"))
        .unwrap();
    let (result, events) = complete(agent.clone(), handle, stream).await;
    assert_eq!(result.unwrap().status, RunStatus::Completed);
    validate_client(&events).await;
    let mut ids = BTreeSet::new();
    let mut nodes = BTreeSet::new();
    for event in &events {
        if let Event::ToolCallResult(result) = event {
            ids.insert(result.tool_call_id.to_string());
            nodes.insert(
                event.base().metadata.as_ref().unwrap()["framework"]["node"]["node_id"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            );
        }
    }
    assert_eq!(ids.len(), 2);
    assert_eq!(nodes, BTreeSet::from(["a".into(), "b".into()]));
    // Two workflow nodes and two model calls per node.
    assert_eq!(types(&events, EventType::StepStarted), 6);
    assert_eq!(types(&events, EventType::StepFinished), 6);
    agent.framework().close().await;
    store.close().await.unwrap();
}

struct Approval;
#[async_trait]
impl ToolPolicy for Approval {
    async fn evaluate(
        &self,
        _: &ToolInvocationContext<'_>,
    ) -> Result<ToolPolicyDecision, AgentError> {
        Ok(ToolPolicyDecision::RequireApproval("Continue?".into()))
    }
}
async fn approval_agent(
    plugin: &AgUiPlugin,
    store: &Arc<SqliteStateStore>,
    count: &Arc<AtomicUsize>,
) -> Agent {
    TestBuilder::default()
        .plugin(plugin.clone())
        .plugin(store.clone().plugin())
        .plugin(AgentLoopPlugin::new(
            WorkflowLoop::new(WorkflowDefinition {
                id: "approval".into(),
                version: 1,
                stages: vec![WorkflowStage::new("solve")],
            })
            .unwrap(),
        ))
        .plugin(ChatModelPlugin::new(ToolModel { barrier: None }))
        .plugin(tools(count.clone(), false))
        .plugin(ToolPolicyPlugin::new("approve", Approval))
        .build()
        .await
        .unwrap()
}

async fn approval_round_trip(approved: bool, legacy: bool) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("approval.db");
    let store = Arc::new(
        SqliteStateStore::open(&path, SqliteOptions::default())
            .await
            .unwrap(),
    );
    let plugin = AgUiPlugin::default();
    let count = Arc::new(AtomicUsize::new(0));
    let agent = approval_agent(&plugin, &store, &count).await;
    let (handle, stream) = agent
        .start_with_output::<AgUiStream>(RunRequest::new("approval", "task"))
        .unwrap();
    let id = handle.run_id;
    let (result, first_events) = complete(agent.clone(), handle, stream).await;
    let result = result.unwrap();
    assert_eq!(result.status, RunStatus::AwaitingInput);
    assert_eq!(count.load(Ordering::SeqCst), 0);
    validate_client(&first_events).await;
    let Event::RunFinished(finished) = first_events.last().unwrap() else {
        panic!()
    };
    let pending = finished.outcome.as_ref().unwrap().interrupts();
    assert_eq!(pending[0].id, result.interactions[0].id.to_string());
    assert_eq!(pending[0].reason, "tool_call");
    let original_id = first_events
        .iter()
        .find_map(|event| match event {
            Event::ToolCallStart(start) => Some(start.tool_call_id.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(pending[0].tool_call_id.as_ref(), Some(&original_id));
    let InteractionKind::Approval { invocation_id } = result.interactions[0].kind else {
        panic!()
    };
    assert_eq!(original_id.as_str(), invocation_id.to_string());

    if legacy {
        let lease = store
            .claim(id, RunId::new_v4(), Duration::from_secs(30))
            .await
            .unwrap();
        let mut checkpoint = store.load(id).await.unwrap();
        let batch = &mut checkpoint.state["cursor"]["Tools"];
        for call in batch["assistant"]["tool_calls"].as_array_mut().unwrap() {
            call.as_object_mut().unwrap().remove("invocation_id");
        }
        for execution in batch["executions"].as_array_mut().unwrap() {
            execution["call"]
                .as_object_mut()
                .unwrap()
                .remove("invocation_id");
        }
        store
            .commit(&lease, checkpoint.revision, checkpoint)
            .await
            .unwrap();
        store.release(&lease).await.unwrap();
    }
    // Restart both the executor and adapter: correlation cannot rely on their maps.
    agent.framework().close().await;
    store.close().await.unwrap();
    let store = Arc::new(
        SqliteStateStore::open(&path, SqliteOptions::default())
            .await
            .unwrap(),
    );
    let plugin = AgUiPlugin::default();
    let agent = approval_agent(&plugin, &store, &count).await;
    let mut request = RunRequest::new("approval", "");
    request.resume = Some(ResumeRequest {
        run_id: id,
        response: Some(InteractionResponse {
            interaction_id: result.interactions[0].id,
            value: json!(approved),
        }),
    });
    let (handle, stream) = agent.start_with_output::<AgUiStream>(request).unwrap();
    let (result, events) = complete(agent.clone(), handle, stream).await;
    assert_eq!(result.unwrap().status, RunStatus::Completed);
    assert_eq!(count.load(Ordering::SeqCst), usize::from(approved));
    validate_client(&events).await;
    assert_eq!(types(&events, EventType::ToolCallStart), 0);
    assert_eq!(types(&events, EventType::ToolCallArgs), 0);
    assert_eq!(types(&events, EventType::ToolCallEnd), 0);
    assert_eq!(types(&events, EventType::ToolCallResult), 1);
    let tool_result = events
        .iter()
        .find_map(|event| match event {
            Event::ToolCallResult(result) => Some(result),
            _ => None,
        })
        .unwrap();
    assert_eq!(tool_result.tool_call_id, original_id);
    let payload: Value = serde_json::from_str(&tool_result.content).unwrap();
    assert_eq!(payload["isError"], !approved);
    let (Event::RunStarted(first), Event::RunStarted(resumed)) = (&first_events[0], &events[0])
    else {
        panic!()
    };
    assert_ne!(first.run_id, resumed.run_id);
    assert_eq!(first.thread_id, resumed.thread_id);

    // One real SDK session consumes both streams, preserving the original card.
    let transport = ag_ui::client::transport::ReplayTransport::with_runs([first_events, events]);
    let mut session = ag_ui::client::Session::<_>::new(transport, "approval");
    let mut run = session.send("task");
    while let Some(update) = run.next().await {
        if let ag_ui::client::Update::Error(error) = update {
            panic!("SDK rejected start: {error}");
        }
    }
    drop(run);
    let pending = session.interrupts()[0].clone();
    let mut resumed = session.resume(&pending, json!(approved));
    while let Some(update) = resumed.next().await {
        if let ag_ui::client::Update::Error(error) = update {
            panic!("SDK rejected resume: {error}");
        }
    }
    drop(resumed);
    assert!(session.interrupts().is_empty());
    assert_eq!(
        session
            .messages()
            .iter()
            .filter_map(|message| match message {
                ag_ui::Message::Assistant(message) => message.tool_calls.as_ref(),
                _ => None,
            })
            .flatten()
            .filter(|call| call.id == original_id)
            .count(),
        1
    );
    assert_eq!(
        session
            .messages()
            .iter()
            .filter(|message| matches!(message,
        ag_ui::Message::Tool(message) if message.tool_call_id == original_id))
            .count(),
        1
    );
    agent.framework().close().await;
    store.close().await.unwrap();
}

#[tokio::test]
async fn workflow_approval_and_resume_preserve_tool_identity_after_restart() {
    approval_round_trip(true, false).await;
}
#[tokio::test]
async fn workflow_denial_answers_the_original_tool_without_execution() {
    approval_round_trip(false, false).await;
}

#[tokio::test]
async fn legacy_checkpoint_restores_call_identity_from_durable_execution() {
    approval_round_trip(true, true).await;
}

struct PresentationLoop {
    invalid_patch: bool,
}
#[async_trait]
impl AgentLoop for PresentationLoop {
    async fn run(
        &self,
        _: &RunRequest,
        context: &RunContext,
        _: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        context.emit_output(OutputEvent::StateSnapshot(json!({"progress":0})));
        context.emit_output(OutputEvent::StateDelta(if self.invalid_patch {
            json!([{"op":"invalid","path":"/progress"}])
        } else {
            json!([{"op":"replace","path":"/progress","value":1}])
        }));
        context.emit_output(OutputEvent::ActivitySnapshot {
            id: "render".into(),
            activity_type: "video".into(),
            content: json!({"progress":0}).as_object().unwrap().clone(),
        });
        context.emit_output(OutputEvent::ActivityDelta {
            id: "render".into(),
            activity_type: "video".into(),
            patch: json!([{"op":"replace","path":"/progress","value":1}]),
        });
        context.emit_output(OutputEvent::Custom {
            name: "host.preview".into(),
            value: json!({"url":"preview"}),
        });
        Ok(RunResult {
            output: "render complete".into(),
            content: "render complete".into(),
            structured: None,
            status: RunStatus::Completed,
            interactions: vec![],
            usage: Usage::default(),
            steps: 0,
            tool_calls: 0,
            external_session: None,
        })
    }
}
#[tokio::test]
async fn state_activity_custom_and_final_only_loop_outputs_work_with_the_sdk_client() {
    let plugin = AgUiPlugin::default();
    let agent = TestBuilder::default()
        .plugin(plugin.clone())
        .plugin(AgentLoopPlugin::new(PresentationLoop {
            invalid_patch: false,
        }))
        .build()
        .await
        .unwrap();
    let (handle, stream) = agent
        .start_with_output::<AgUiStream>(RunRequest::new("video", "render"))
        .unwrap();
    let (result, events) = complete(agent.clone(), handle, stream).await;
    assert!(result.is_ok());
    validate_client(&events).await;
    assert_eq!(text(&events), "render complete");
    assert_eq!(types(&events, EventType::StateDelta), 1);
    assert_eq!(types(&events, EventType::ActivityDelta), 1);
    agent.framework().close().await;
}

struct CollaborationPresentationLoop;

#[async_trait]
impl AgentLoop for CollaborationPresentationLoop {
    async fn run(
        &self,
        _: &RunRequest,
        context: &RunContext,
        _: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let completed_child = RunId::new_v4();
        context
            .emit(AgentEvent::Collaboration(Box::new(
                CollaborationEvent::ChildRunStarted {
                    child_run_id: completed_child,
                    session_id: "child-completed".into(),
                    depth: 1,
                },
            )))
            .await;
        context
            .emit(AgentEvent::Collaboration(Box::new(
                CollaborationEvent::ChildRunFinished {
                    child_run_id: completed_child,
                    status: RunStatus::Completed,
                    interactions: vec![],
                },
            )))
            .await;

        let interaction = InteractionRequest::input(
            "Choose a target",
            json!({"type":"string","enum":["staging","production"]}),
        )?;
        let waiting_child = RunId::new_v4();
        context
            .emit(AgentEvent::Collaboration(Box::new(
                CollaborationEvent::ChildRunStarted {
                    child_run_id: waiting_child,
                    session_id: "child-waiting".into(),
                    depth: 1,
                },
            )))
            .await;
        context
            .emit(AgentEvent::Collaboration(Box::new(
                CollaborationEvent::ChildRunFinished {
                    child_run_id: waiting_child,
                    status: RunStatus::AwaitingInput,
                    interactions: vec![interaction.clone()],
                },
            )))
            .await;

        let thread_id = "thread-collaboration".to_owned();
        let thread_run_id = RunId::new_v4();
        context
            .emit(AgentEvent::Collaboration(Box::new(
                CollaborationEvent::ThreadCreated {
                    summary: ThreadSummary {
                        thread_id: thread_id.clone(),
                        title: Some("worker".into()),
                        status: ThreadStatus::Running,
                        latest_run_id: Some(thread_run_id),
                    },
                },
            )))
            .await;
        context
            .emit(AgentEvent::Collaboration(Box::new(
                CollaborationEvent::ThreadRunStarted {
                    thread_id: thread_id.clone(),
                    run_id: thread_run_id,
                    cause: ThreadRunCause::Created,
                    interaction_id: None,
                },
            )))
            .await;
        context
            .emit(AgentEvent::Collaboration(Box::new(
                CollaborationEvent::ThreadRunFinished {
                    thread_id: thread_id.clone(),
                    run_id: thread_run_id,
                    status: ThreadStatus::AwaitingInput,
                    interactions: vec![interaction.clone()],
                    error: None,
                },
            )))
            .await;
        context
            .emit(AgentEvent::Collaboration(Box::new(
                CollaborationEvent::ThreadClosed { thread_id },
            )))
            .await;

        Ok(RunResult {
            output: String::new(),
            content: Content::default(),
            structured: None,
            status: RunStatus::AwaitingInput,
            interactions: vec![interaction],
            usage: Usage::default(),
            steps: 0,
            tool_calls: 0,
            external_session: None,
        })
    }
}

#[tokio::test]
async fn collaboration_events_map_to_subagent_lifecycle_and_thread_custom_events() {
    let agent = TestBuilder::default()
        .plugin(AgUiPlugin::default())
        .plugin(AgentLoopPlugin::new(CollaborationPresentationLoop))
        .build()
        .await
        .unwrap();
    let (handle, stream) = agent
        .start_with_output::<AgUiStream>(RunRequest::new("collaboration", "delegate"))
        .unwrap();
    let (result, events) = complete(agent.clone(), handle, stream).await;
    assert_eq!(result.unwrap().status, RunStatus::AwaitingInput);
    validate_client(&events).await;
    assert_eq!(types(&events, EventType::SubagentStarted), 2);
    assert_eq!(types(&events, EventType::SubagentFinished), 2);
    assert_eq!(types(&events, EventType::SubagentError), 0);
    let suspended = events.iter().find_map(|event| match event {
        Event::SubagentFinished(event)
            if event
                .outcome
                .as_ref()
                .is_some_and(ag_ui::SubagentOutcome::is_suspended) =>
        {
            Some(event)
        }
        _ => None,
    });
    assert_eq!(
        suspended
            .and_then(|event| event.outcome.as_ref())
            .map(ag_ui::SubagentOutcome::interrupt_ids)
            .map(<[_]>::len),
        Some(1)
    );
    let custom_names = events
        .iter()
        .filter_map(|event| match event {
            Event::Custom(event) => Some(event.name.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        custom_names,
        [
            "framework.subagent.interactions",
            "framework.thread.created",
            "framework.thread.run.started",
            "framework.thread.run.finished",
            "framework.thread.closed",
        ]
    );
    let thread_finished = events.iter().find_map(|event| match event {
        Event::Custom(event) if event.name == "framework.thread.run.finished" => Some(&event.value),
        _ => None,
    });
    assert_eq!(
        thread_finished
            .and_then(|value| value["interactions"].as_array())
            .map(Vec::len),
        Some(1)
    );
    agent.framework().close().await;
}

#[tokio::test]
async fn malformed_patch_fails_the_stream_without_changing_execution() {
    let plugin = AgUiPlugin::default();
    let agent = TestBuilder::default()
        .plugin(plugin.clone())
        .plugin(AgentLoopPlugin::new(PresentationLoop {
            invalid_patch: true,
        }))
        .build()
        .await
        .unwrap();
    let (handle, stream) = agent
        .start_with_output::<AgUiStream>(RunRequest::new("state", "task"))
        .unwrap();
    let (result, events) = complete(agent.clone(), handle, stream).await;
    assert!(result.is_ok());
    assert_eq!(code(&events), Some("AGUI_INVALID_PATCH"));
    assert_eq!(types(&events, EventType::RunFinished), 0);
    agent.framework().close().await;
}

#[tokio::test]
async fn overflow_reports_incomplete_output_and_never_fabricates_run_success() {
    let plugin = AgUiPlugin::new(AgUiOptions {
        capacity: 1,
        ..Default::default()
    })
    .unwrap();
    let agent = TestBuilder::default()
        .plugin(plugin.clone())
        .plugin(ChatModelPlugin::new(Streaming))
        .build()
        .await
        .unwrap();
    let (handle, stream) = agent
        .start_with_output::<AgUiStream>(RunRequest::new("overflow", "task"))
        .unwrap();
    assert_eq!(
        agent.wait(handle).await.unwrap().status,
        RunStatus::Completed
    );
    let events = collect(stream).await;
    assert_eq!(types(&events, EventType::RunStarted), 1);
    assert_eq!(code(&events), Some("AGUI_STREAM_OVERFLOW"));
    assert_eq!(types(&events, EventType::RunFinished), 0);
    agent.framework().close().await;
}

struct Mismatch;
#[async_trait]
impl ChatModel for Mismatch {
    async fn generate(
        &self,
        _model_id: &str,
        _: ChatRequest,
        stream: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        stream.text_delta("visible text").await;
        Ok(ChatResponse::text("different final text"))
    }
}
#[tokio::test]
async fn rejected_final_text_still_closes_previously_visible_messages() {
    let plugin = AgUiPlugin::default();
    let agent = TestBuilder::default()
        .plugin(plugin.clone())
        .plugin(ChatModelPlugin::new(Mismatch))
        .build()
        .await
        .unwrap();
    let (handle, stream) = agent
        .start_with_output::<AgUiStream>(RunRequest::new("mismatch", "task"))
        .unwrap();
    let (result, events) = complete(agent.clone(), handle, stream).await;
    assert!(result.is_ok());
    assert_eq!(text(&events), "visible text");
    assert_eq!(types(&events, EventType::TextMessageStart), 1);
    assert_eq!(types(&events, EventType::TextMessageEnd), 1);
    assert_eq!(code(&events), Some("AGUI_TEXT_MISMATCH"));
    agent.framework().close().await;
}

struct Gated {
    entered: Arc<Notify>,
}
#[async_trait]
impl ChatModel for Gated {
    async fn generate(
        &self,
        _model_id: &str,
        _: ChatRequest,
        stream: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        stream.text_delta("partial").await;
        self.entered.notify_one();
        std::future::pending().await
    }
}
#[tokio::test]
async fn cloned_control_cancellation_closes_the_stream_once() {
    let entered = Arc::new(Notify::new());
    let plugin = AgUiPlugin::default();
    let agent = TestBuilder::default()
        .plugin(plugin.clone())
        .plugin(ChatModelPlugin::new(Gated {
            entered: entered.clone(),
        }))
        .build()
        .await
        .unwrap();
    let (handle, stream) = agent
        .start_with_output::<AgUiStream>(RunRequest::new("cancel", "task"))
        .unwrap();
    let control = agent.control(&handle).unwrap();
    let completion = tokio::spawn(complete(agent.clone(), handle, stream));
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    agent.cancel(&control).unwrap();
    let (result, events) = completion.await.unwrap();
    assert!(matches!(result, Err(AgentError::Cancelled)));
    assert_eq!(text(&events), "partial");
    assert_eq!(types(&events, EventType::TextMessageEnd), 1);
    assert_eq!(types(&events, EventType::RunError), 1);
    assert_eq!(code(&events), Some("AGENT_CANCELLED"));
    agent.framework().close().await;
}

#[tokio::test]
async fn missing_plugin_is_rejected_before_an_unobserved_run_can_continue() {
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(Streaming))
        .build()
        .await
        .unwrap();
    assert!(matches!(
        agent.start_with_output::<AgUiStream>(RunRequest::new("missing", "task")),
        Err(AgentError::MissingService(_))
    ));
    agent.framework().close().await;
}

struct StartProbe(Arc<AtomicUsize>);
#[async_trait]
impl EventSink for StartProbe {
    fn on_published(&self, _: &RunContext, event: &EventEnvelope) {
        if matches!(event.event, AgentEvent::RunStarted { .. }) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    async fn on_event(&self, _: &RunContext, _: &AgentEvent) {}
}
#[tokio::test]
async fn configured_plugin_is_prepared_by_agent_without_retaining_its_instance() {
    let started = Arc::new(AtomicUsize::new(0));
    let agent = TestBuilder::default()
        .plugin(AgUiPlugin::default())
        .plugin(EventPlugin::new("probe", StartProbe(started.clone())))
        .plugin(ChatModelPlugin::new(Streaming))
        .build()
        .await
        .unwrap();
    let mut handle = agent.start(RunRequest::new("configured", "task")).unwrap();
    let stream = agent.output::<AgUiStream>(&mut handle).unwrap();
    assert!(agent.output::<AgUiStream>(&mut handle).is_err());
    let (result, events) = complete(agent.clone(), handle, stream).await;
    assert!(result.is_ok());
    validate_client(&events).await;
    assert_eq!(started.load(Ordering::SeqCst), 1);
    agent.framework().close().await;
}

#[tokio::test]
async fn escaped_session_id_is_checked_before_admission_and_valid_ids_keep_run_start() {
    let plugin = AgUiPlugin::new(AgUiOptions {
        max_event_bytes: 1024,
        ..Default::default()
    })
    .unwrap();
    let started = Arc::new(AtomicUsize::new(0));
    let agent = TestBuilder::default()
        .plugin(plugin.clone())
        .plugin(EventPlugin::new("probe", StartProbe(started.clone())))
        .plugin(ChatModelPlugin::new(Streaming))
        .build()
        .await
        .unwrap();
    for id in ["\u{0001}".repeat(200), "\"".repeat(240), "\\".repeat(240)] {
        assert!(matches!(
            agent.start_with_output::<AgUiStream>(RunRequest::new(id, "task")),
            Err(AgentError::InvalidConfiguration(_))
        ));
    }
    assert_eq!(started.load(Ordering::SeqCst), 0);
    // Escaping itself is allowed when both the source and protocol frame fit.
    for id in ["normal", "会话\\\"-\u{0001}"] {
        let (handle, stream) = agent
            .start_with_output::<AgUiStream>(RunRequest::new(id, "task"))
            .unwrap();
        let (result, events) = complete(agent.clone(), handle, stream).await;
        assert!(result.is_ok());
        assert_eq!(code(&events), None);
        validate_client(&events).await;
        let Event::RunStarted(start) = &events[0] else {
            panic!()
        };
        assert_eq!(start.thread_id.as_str(), id);
        assert!(
            events
                .iter()
                .all(|event| serde_json::to_vec(event).unwrap().len() <= 1024)
        );
    }
    assert_eq!(started.load(Ordering::SeqCst), 2);
    agent.framework().close().await;
}

#[test]
fn dropping_the_executor_runtime_without_terminal_publication_ends_the_stream() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (agent, handle, stream) = runtime.block_on(async {
        let entered = Arc::new(Notify::new());
        let plugin = AgUiPlugin::default();
        let agent = TestBuilder::default()
            .plugin(plugin.clone())
            .plugin(ChatModelPlugin::new(Gated {
                entered: entered.clone(),
            }))
            .build()
            .await
            .unwrap();
        let (handle, stream) = agent
            .start_with_output::<AgUiStream>(RunRequest::new("aborted", "task"))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        (agent, handle, stream)
    });
    drop(runtime);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let (result, events) = complete(agent.clone(), handle, stream).await;
        assert!(result.is_err());
        assert_eq!(code(&events), Some("AGUI_STREAM_INTERRUPTED"));
        assert_eq!(types(&events, EventType::TextMessageStart), 1);
        assert_eq!(types(&events, EventType::TextMessageEnd), 1);
        agent.framework().close().await;
    });
}

struct RetryModel(AtomicUsize);
#[async_trait]
impl ChatModel for RetryModel {
    async fn generate(
        &self,
        _model_id: &str,
        _: ChatRequest,
        stream: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(ModelError::Transient(
                "retry this private provider error".into(),
            ));
        }
        stream.text_delta("recovered").await;
        Ok(ChatResponse::text("recovered"))
    }
}
#[tokio::test]
async fn a_model_retry_without_visible_output_does_not_create_phantom_messages() {
    let plugin = AgUiPlugin::default();
    let agent = TestBuilder::default()
        .plugin(plugin.clone())
        .plugin(ChatModelPlugin::new(RetryModel(AtomicUsize::new(0))))
        .build()
        .await
        .unwrap();
    let (handle, stream) = agent
        .start_with_output::<AgUiStream>(RunRequest::new("retry", "task"))
        .unwrap();
    let (result, events) = complete(agent.clone(), handle, stream).await;
    assert!(result.is_ok());
    validate_client(&events).await;
    assert_eq!(types(&events, EventType::TextMessageStart), 1);
    assert_eq!(text(&events), "recovered");
    assert!(
        !serde_json::to_string(&events)
            .unwrap()
            .contains("private provider error")
    );
    agent.framework().close().await;
}

struct Oversized;
#[async_trait]
impl ChatModel for Oversized {
    async fn generate(
        &self,
        _model_id: &str,
        _: ChatRequest,
        stream: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        stream.text_delta("visible").await;
        let huge = "x".repeat(4096);
        stream.text_delta(&huge).await;
        Ok(ChatResponse::text(format!("visible{huge}")))
    }
}
#[tokio::test]
async fn oversized_payloads_fail_explicitly_after_closing_visible_text() {
    let plugin = AgUiPlugin::new(AgUiOptions {
        max_event_bytes: 1024,
        ..Default::default()
    })
    .unwrap();
    let agent = TestBuilder::default()
        .plugin(plugin.clone())
        .plugin(ChatModelPlugin::new(Oversized))
        .build()
        .await
        .unwrap();
    let (handle, stream) = agent
        .start_with_output::<AgUiStream>(RunRequest::new("large", "task"))
        .unwrap();
    let (result, events) = complete(agent.clone(), handle, stream).await;
    assert!(result.is_ok());
    assert_eq!(text(&events), "visible");
    assert_eq!(types(&events, EventType::TextMessageEnd), 1);
    assert_eq!(code(&events), Some("AGUI_EVENT_TOO_LARGE"));
    agent.framework().close().await;
}

struct Failing;
#[async_trait]
impl ChatModel for Failing {
    async fn generate(
        &self,
        _model_id: &str,
        _: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        Err(ModelError::Fatal("private credential detail".into()))
    }
}
#[tokio::test]
async fn run_error_details_are_hidden_unless_explicitly_enabled() {
    for visible in [false, true] {
        let plugin = AgUiPlugin::new(AgUiOptions {
            include_error_details: visible,
            ..Default::default()
        })
        .unwrap();
        let agent = TestBuilder::default()
            .plugin(plugin.clone())
            .plugin(ChatModelPlugin::new(Failing))
            .build()
            .await
            .unwrap();
        let (handle, stream) = agent
            .start_with_output::<AgUiStream>(RunRequest::new("error", "task"))
            .unwrap();
        let (result, events) = complete(agent.clone(), handle, stream).await;
        assert!(result.is_err());
        assert_eq!(code(&events), Some("AGENT_RUN_FAILED"));
        assert_eq!(
            serde_json::to_string(&events)
                .unwrap()
                .contains("private credential detail"),
            visible
        );
        agent.framework().close().await;
    }
}

struct SlowSink;
#[async_trait]
impl EventSink for SlowSink {
    async fn on_event(&self, _: &RunContext, _: &AgentEvent) {
        std::future::pending().await
    }
}
struct ManyChunks;
#[async_trait]
impl ChatModel for ManyChunks {
    async fn generate(
        &self,
        _model_id: &str,
        _: ChatRequest,
        stream: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        for _ in 0..400 {
            stream.text_delta("x").await;
        }
        Ok(ChatResponse::text("x".repeat(400)))
    }
}
#[tokio::test]
async fn slow_observers_cannot_drop_agui_messages_or_the_terminal_event() {
    let plugin = AgUiPlugin::new(AgUiOptions {
        capacity: 1024,
        ..Default::default()
    })
    .unwrap();
    let agent = TestBuilder::default()
        .plugin(EventPlugin::new("slow", SlowSink))
        .plugin(plugin.clone())
        .plugin(ChatModelPlugin::new(ManyChunks))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("slow", "task");
    // Allow SQLite fsync/release under concurrent tests; the sink still outlives this budget.
    request.limits.cleanup_timeout = Duration::from_millis(100);
    let (handle, stream) = agent.start_with_output::<AgUiStream>(request).unwrap();
    let (result, events) = complete(agent.clone(), handle, stream).await;
    assert!(result.is_ok());
    validate_client(&events).await;
    assert_eq!(types(&events, EventType::TextMessageContent), 400);
    assert_eq!(text(&events), "x".repeat(400));
    agent.framework().close().await;
}

struct AuxiliaryLoop;
#[async_trait]
impl AgentLoop for AuxiliaryLoop {
    fn requires_model(&self) -> bool {
        true
    }
    async fn run(
        &self,
        request: &RunRequest,
        context: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        services
            .models
            .as_ref()
            .unwrap()
            .invoke_auxiliary(
                &ModelRef::new("default", "default"),
                ChatRequest {
                    options: GenerationOptions::default(),
                    messages: vec![Message::user("summarize")],
                    tools: vec![],
                    extensions: Extensions::default(),
                },
                "summary",
                &request.limits,
                context,
            )
            .await?;
        Ok(RunResult {
            output: "public answer".into(),
            content: "public answer".into(),
            structured: None,
            status: RunStatus::Completed,
            interactions: vec![],
            usage: context.usage(),
            steps: 0,
            tool_calls: 0,
            external_session: None,
        })
    }
}
#[tokio::test]
async fn auxiliary_model_text_and_reasoning_never_enter_the_ui_stream() {
    let plugin = AgUiPlugin::default();
    let agent = TestBuilder::default()
        .plugin(plugin.clone())
        .plugin(AgentLoopPlugin::new(AuxiliaryLoop))
        .plugin(ChatModelPlugin::new(Streaming))
        .build()
        .await
        .unwrap();
    let (handle, stream) = agent
        .start_with_output::<AgUiStream>(RunRequest::new("summary", "task"))
        .unwrap();
    let (result, events) = complete(agent.clone(), handle, stream).await;
    assert!(result.is_ok());
    validate_client(&events).await;
    assert_eq!(text(&events), "public answer");
    assert_eq!(types(&events, EventType::ReasoningMessageStart), 0);
    assert!(!serde_json::to_string(&events).unwrap().contains("推理摘要"));
    agent.framework().close().await;
}

struct SharedResponseModel {
    response: ChatResponse,
    barrier: Barrier,
}
#[async_trait]
impl ChatModel for SharedResponseModel {
    async fn generate(
        &self,
        _model_id: &str,
        _: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        self.barrier.wait().await;
        Ok(self.response.clone())
    }
}
struct ParallelModels;
#[async_trait]
impl AgentLoop for ParallelModels {
    async fn run(
        &self,
        request: &RunRequest,
        context: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let model = services.models.as_ref().unwrap();
        let mut first = services.conversations.open("parallel-first").await?;
        let mut second = services.conversations.open("parallel-second").await?;
        first.append(&[Message::user("task one")]).await?;
        second.append(&[Message::user("task two")]).await?;
        let (first, second) = tokio::join!(
            model.invoke(
                ModelCall {
                    stage: "first",
                    step: 1,
                    instructions: &[],
                    tools: &[],
                    limits: &request.limits,
                    options: &request.generation
                },
                first.as_mut(),
                context
            ),
            model.invoke(
                ModelCall {
                    stage: "second",
                    step: 1,
                    instructions: &[],
                    tools: &[],
                    limits: &request.limits,
                    options: &request.generation
                },
                second.as_mut(),
                context
            ),
        );
        for response in [first?, second?] {
            for call in response.tool_calls {
                context.emit_output(OutputEvent::ToolResult {
                    call,
                    output: ToolOutput::text("result"),
                });
            }
        }
        Ok(RunResult {
            output: "done".into(),
            content: "done".into(),
            structured: None,
            status: RunStatus::Completed,
            interactions: vec![],
            usage: context.usage(),
            steps: 2,
            tool_calls: 2,
            external_session: None,
        })
    }
}
#[tokio::test]
async fn parallel_calls_in_custom_loop_preserve_distinct_invocations() {
    let plugin = AgUiPlugin::default();
    let agent = TestBuilder::default()
        .plugin(plugin.clone())
        .plugin(ChatModelPlugin::new(SharedResponseModel {
            response: ChatResponse::tool_calls(vec![ToolCall::new(
                "same-provider-id",
                "work",
                json!({}),
            )]),
            barrier: Barrier::new(2),
        }))
        .plugin(AgentLoopPlugin::new(ParallelModels))
        .build()
        .await
        .unwrap();
    let (handle, stream) = agent
        .start_with_output::<AgUiStream>(RunRequest::new("parallel", "task"))
        .unwrap();
    let (result, events) = complete(agent.clone(), handle, stream).await;
    assert_eq!(result.unwrap().status, RunStatus::Completed);
    assert_eq!(
        code(&events),
        None,
        "valid parallel model responses terminated the UI stream"
    );
    assert_eq!(types(&events, EventType::ToolCallResult), 2);
    let proposals: BTreeSet<_> = events
        .iter()
        .filter_map(|event| match event {
            Event::ToolCallStart(call) => Some(call.tool_call_id.clone()),
            _ => None,
        })
        .collect();
    let results: BTreeSet<_> = events
        .iter()
        .filter_map(|event| match event {
            Event::ToolCallResult(call) => Some(call.tool_call_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(proposals.len(), 2);
    assert_eq!(proposals, results);
    validate_client(&events).await;
    agent.framework().close().await;
}

struct FinalAfterProgress;
#[async_trait]
impl AgentLoop for FinalAfterProgress {
    async fn run(
        &self,
        _: &RunRequest,
        context: &RunContext,
        _: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        context.emit_output(OutputEvent::MessageStarted {
            message_id: "progress".into(),
        });
        context.emit_output(OutputEvent::MessageFinished {
            message_id: "progress".into(),
            content: "Preparing the render".into(),
            structured: None,
            tool_calls: vec![],
        });
        Ok(RunResult {
            output: "Video is ready".into(),
            content: "Video is ready".into(),
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
#[tokio::test]
async fn final_only_output_after_progress_is_visible() {
    let plugin = AgUiPlugin::default();
    let agent = TestBuilder::default()
        .plugin(plugin.clone())
        .plugin(AgentLoopPlugin::new(FinalAfterProgress))
        .build()
        .await
        .unwrap();
    let (handle, stream) = agent
        .start_with_output::<AgUiStream>(RunRequest::new("video-final", "task"))
        .unwrap();
    let (result, events) = complete(agent.clone(), handle, stream).await;
    let result = result.unwrap();
    assert_eq!(code(&events), None);
    assert!(
        text(&events).contains(&result.output),
        "visible text: {:?}, final result: {:?}",
        text(&events),
        result.output
    );
    agent.framework().close().await;
}

#[tokio::test]
async fn structured_sender_provenance_survives_agui_live_and_replay_transport() {
    let agent = TestBuilder::default()
        .plugin(AgUiPlugin::default())
        .plugin(ChatModelPlugin::new(Streaming))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("dude-work", "task");
    let provenance = MessageProvenance {
        sender: MessageSender {
            kind: "dude".into(),
            id: Some("main".into()),
            display_name: Some("Main Dude".into()),
            avatar_url: Some("avatar.svg".into()),
        },
        source: Some("heartbeat".into()),
        correlation: Some(json!({"eventId":"event-1"})),
    };
    request.assistant_provenance = Some(provenance.clone());
    let (handle, stream) = agent.start_with_output::<AgUiStream>(request).unwrap();
    let (result, events) = complete(agent.clone(), handle, stream).await;
    result.unwrap();
    let custom = events
        .iter()
        .find_map(|event| match event {
            Event::Custom(event) if event.name == "framework.message.provenance" => Some(event),
            _ => None,
        })
        .expect("missing structured attribution");
    assert_eq!(
        custom.value["provenance"],
        serde_json::to_value(provenance).unwrap()
    );
    let message_id = events
        .iter()
        .find_map(|event| match event {
            Event::TextMessageStart(event) => Some(event.message_id.to_string()),
            _ => None,
        })
        .unwrap();
    assert_eq!(custom.value["messageId"], json!(message_id));
    assert_eq!(custom.value["sessionId"], json!("dude-work"));
    assert_eq!(
        custom.base.metadata.as_ref().unwrap()["framework"]["sessionId"],
        json!("dude-work")
    );
    validate_client(&events).await;
    assert!(agent.framework().close().await.is_success());
}
