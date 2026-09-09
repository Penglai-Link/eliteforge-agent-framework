use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_agui::*;
use eliteforge_agent_official_plugin_storage_sqlite::*;
use eliteforge_agent_official_plugin_tools_human::*;
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

#[derive(Clone)]
struct Script {
    requests: Arc<Mutex<Vec<ChatRequest>>>,
    first: Option<ToolCall>,
}
#[async_trait]
impl ChatModel for Script {
    async fn generate(
        &self,
        _model_id: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        let mut seen = self.requests.lock().unwrap();
        let mut response = ChatResponse::text("done");
        if seen.is_empty()
            && let Some(call) = &self.first
        {
            response.tool_calls = vec![call.clone()];
        }
        seen.push(request);
        Ok(response)
    }
}
struct Counted(Arc<Mutex<Vec<Value>>>);
#[async_trait]
impl Tool for Counted {
    async fn call(&self, arguments: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        self.0.lock().unwrap().push(arguments);
        Ok(ToolOutput::text("executed"))
    }
}
struct Approve;
#[async_trait]
impl ToolPolicy for Approve {
    async fn evaluate(
        &self,
        _: &ToolInvocationContext<'_>,
    ) -> Result<ToolPolicyDecision, AgentError> {
        Ok(ToolPolicyDecision::RequireApproval(
            "Execute this call?".into(),
        ))
    }
}
fn questions() -> Value {
    json!({"intro":"Choose the video format", "questions":[
        {"question":"Aspect ratio?","options":[{"label":"Landscape"},{"label":"Portrait"}]},
        {"question":"Any preferences?","options":[{"label":"Quiet"},{"label":"Energetic"}]},
        {"question":"Music?","options":[{"label":"Yes"},{"label":"No"}]}
    ]})
}
fn selected(request: &InteractionRequest) -> Value {
    let presentation: AskUserPresentation =
        serde_json::from_value(request.presentation.clone().unwrap()).unwrap();
    let q = &presentation.questions;
    json!({"answers":{
        q[0].id.to_string():{"option_id":q[0].options[1].id},
        q[1].id.to_string():{"text":"Use captions"},
        q[2].id.to_string():{"skip":true}
    }})
}
async fn build(
    path: &Path,
    mode: &str,
    first: Option<ToolCall>,
    seen: Arc<Mutex<Vec<ChatRequest>>>,
    count: Arc<Mutex<Vec<Value>>>,
    approval: bool,
) -> Agent {
    let mut builder = AgentBuilder::new().plugin(SqliteStoragePlugin::new(path))
        .plugin(ChatModelPlugin::new(Script { first, requests: seen })).plugin(HumanInteractionPlugin)
        .plugin(ToolProviderPlugin::new("work", StaticToolProvider::new(vec![ToolBinding::new(
            ToolDescriptor::new("work", "work", "work", json!({"type":"object","required":["value"],"properties":{"value":{"type":"integer"}},"additionalProperties":false})), Counted(count)
        )]))).plugin(AgUiPlugin::default());
    if approval {
        builder = builder.plugin(ToolPolicyPlugin::new("approve", Approve));
    }
    if mode == "workflow" {
        builder = builder.plugin(AgentLoopPlugin::new(
            WorkflowLoop::new(WorkflowDefinition {
                id: "questions".into(),
                version: 1,
                stages: vec![WorkflowStage::new("react")],
            })
            .unwrap(),
        ));
    } else if mode == "dag" {
        builder = builder.plugin(AgentLoopPlugin::new(
            DagWorkflowLoop::new(DagWorkflowDefinition {
                id: "questions".into(),
                version: 1,
                nodes: vec![DagNode::react(WorkflowStage::new("react"))],
                edges: vec![],
                max_parallel_nodes: 1,
            })
            .unwrap(),
        ));
    }
    builder.build().await.unwrap()
}
#[tokio::test]
async fn all_loops_persist_questions_answers_and_agui_interrupts_across_restart() {
    for mode in ["react", "workflow", "dag"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.db");
        let seen = Arc::new(Mutex::new(vec![]));
        let calls = Arc::new(Mutex::new(vec![]));
        let agent = build(
            &path,
            mode,
            Some(ToolCall::new("ask-1", "ask_user", questions())),
            seen.clone(),
            calls.clone(),
            false,
        )
        .await;
        let (handle, mut stream) = agent
            .start_with_output::<AgUiStream>(RunRequest::new("chat", "Edit my video"))
            .unwrap();
        let result = agent.wait(handle).await.unwrap();
        assert_eq!(result.status, RunStatus::AwaitingInput);
        let question = result.interactions[0].clone();
        let mut events = vec![];
        while let Some(event) = stream.recv().await {
            events.push(serde_json::to_value(event).unwrap());
        }
        let end = events.last().unwrap();
        assert_eq!(end["type"], "RUN_FINISHED");
        assert!(end.to_string().contains("ask_user"));
        let records = agent.pending_interactions("chat").await.unwrap();
        assert_eq!(records.len(), 1);
        assert!(records[0].tool_call.is_some());
        assert!(agent.close().await.is_success());
        drop(agent);
        let agent = build(&path, mode, None, seen.clone(), calls, false).await;
        assert_eq!(
            agent.pending_interactions("chat").await.unwrap()[0].request,
            question
        );
        assert!(
            agent
                .answer("other", question.id, selected(&question))
                .await
                .is_err()
        );
        assert!(
            agent
                .answer("chat", question.id, json!({"answers":{}}))
                .await
                .is_err()
        );
        let answer = selected(&question);
        let resume = agent
            .answer("chat", question.id, answer.clone())
            .await
            .unwrap();
        // Crash between answer acceptance and continuation is safe.
        assert!(agent.close().await.is_success());
        drop(agent);
        let agent = build(&path, mode, None, seen.clone(), Arc::default(), false).await;
        let mut recovery = resume.clone();
        recovery.resume.as_mut().unwrap().response = None;
        let (resumed_handle, mut resumed_stream) =
            agent.start_with_output::<AgUiStream>(recovery).unwrap();
        let final_result = agent.wait(resumed_handle).await.unwrap();
        let mut resumed_events = vec![];
        while let Some(event) = resumed_stream.recv().await {
            resumed_events.push(serde_json::to_value(event).unwrap());
        }
        assert_eq!(resumed_events.last().unwrap()["type"], "RUN_FINISHED");
        assert_ne!(events[0]["runId"], resumed_events[0]["runId"]);
        assert!(
            !resumed_events
                .last()
                .unwrap()
                .to_string()
                .contains("input_required")
        );
        assert_eq!(final_result.status, RunStatus::Completed, "{mode}");
        assert!(agent.pending_interactions("chat").await.unwrap().is_empty());
        let record = agent.interaction("chat", question.id).await.unwrap();
        assert!(record.answered_at_unix_ms.is_some());
        assert_eq!(record.response.unwrap().value, answer);
        assert_eq!(
            seen.lock().unwrap().len(),
            2,
            "question tool must not be replayed"
        );
        let messages = seen.lock().unwrap()[1].messages.clone();
        assert!(messages.iter().any(|m| {
            m.role == Role::User
                && serde_json::to_string(&m.content)
                    .unwrap()
                    .contains("Use captions")
        }));
        assert!(agent.close().await.is_success());
    }
}

#[tokio::test]
async fn react_approval_executes_only_original_arguments_and_identical_resume_is_idempotent() {
    for approved in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.db");
        let seen = Arc::new(Mutex::new(vec![]));
        let calls = Arc::new(Mutex::new(vec![]));
        let agent = build(
            &path,
            "react",
            Some(ToolCall::new("call-1", "work", json!({"value":17}))),
            seen.clone(),
            calls.clone(),
            true,
        )
        .await;
        let result = agent.run(RunRequest::new("s", "work")).await.unwrap();
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(result.status, RunStatus::AwaitingInput);
        let id = result.interactions[0].id;
        let record = agent.interaction("s", id).await.unwrap();
        assert_eq!(record.tool_call.unwrap().arguments, json!({"value":17}));
        agent.close().await;
        drop(agent);
        let agent = build(&path, "react", None, seen.clone(), calls.clone(), true).await;
        let resume = agent.answer("s", id, json!(approved)).await.unwrap();
        assert!(agent.answer("s", id, json!(!approved)).await.is_err());
        let result = agent.run(resume.clone()).await.unwrap();
        assert_eq!(result.status, RunStatus::Completed);
        assert_eq!(calls.lock().unwrap().len(), usize::from(approved));
        if approved {
            assert_eq!(calls.lock().unwrap()[0], json!({"value":17}));
        }
        agent.run(resume).await.unwrap();
        assert_eq!(seen.lock().unwrap().len(), 2);
        assert_eq!(calls.lock().unwrap().len(), usize::from(approved));
        agent.close().await;
    }
}

#[tokio::test]
async fn missing_storage_and_memory_paths_fail_build() {
    assert!(
        AgentBuilder::new()
            .plugin(ChatModelPlugin::new(Script {
                first: None,
                requests: Arc::default()
            }))
            .build()
            .await
            .is_err()
    );
    assert!(
        AgentBuilder::new()
            .plugin(SqliteStoragePlugin::new(":memory:"))
            .build()
            .await
            .is_err()
    );
}

struct FailFinalAppend {
    inner: eliteforge_agent_memory::SqliteConversationStore,
    fail: Arc<std::sync::atomic::AtomicBool>,
}
struct FailFinalSession {
    inner: Box<dyn ConversationSession>,
    fail: Arc<std::sync::atomic::AtomicBool>,
}
#[async_trait]
impl ConversationStore for FailFinalAppend {
    async fn open(&self, session: &str) -> Result<Box<dyn ConversationSession>, ConversationError> {
        Ok(Box::new(FailFinalSession {
            inner: self.inner.open(session).await?,
            fail: self.fail.clone(),
        }))
    }
}
#[async_trait]
impl ConversationSession for FailFinalSession {
    async fn snapshot(&mut self) -> Result<ConversationSnapshot, ConversationError> {
        self.inner.snapshot().await
    }
    async fn append(&mut self, messages: &[Message]) -> Result<(), ConversationError> {
        if messages.iter().any(|m| m.role == Role::Assistant)
            && self.fail.swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(ConversationError::Other(
                "injected transcript write failure".into(),
            ));
        }
        self.inner.append(messages).await
    }
    async fn finish(&mut self, outcome: ConversationOutcome) -> Result<(), ConversationError> {
        self.inner.finish(outcome).await
    }
}
#[tokio::test]
async fn terminal_resume_repairs_transcript_without_repeating_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sync.db");
    let state = Arc::new(
        eliteforge_agent_state_sqlite::SqliteStateStore::open(&path, Default::default())
            .await
            .unwrap(),
    );
    let conversations =
        eliteforge_agent_memory::SqliteConversationStore::open(&path, Default::default())
            .await
            .unwrap();
    let seen = Arc::new(Mutex::new(vec![]));
    let agent = AgentBuilder::new()
        .plugin(state.clone().plugin())
        .plugin(ConversationStorePlugin::new(FailFinalAppend {
            inner: conversations.clone(),
            fail: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }))
        .plugin(ChatModelPlugin::new(Script {
            requests: seen.clone(),
            first: None,
        }))
        .build()
        .await
        .unwrap();
    let handle = agent.start(RunRequest::new("s", "hello")).unwrap();
    let run_id = agent.control(&handle).unwrap().run_id();
    assert!(agent.wait(handle).await.is_err());
    let mut check = conversations.open("s").await.unwrap();
    assert_eq!(check.snapshot().await.unwrap().messages.len(), 1);
    check.finish(ConversationOutcome::Stopped).await.unwrap();
    let mut resume = RunRequest::new("s", "");
    resume.resume = Some(ResumeRequest {
        run_id,
        response: None,
        resolutions: vec![],
    });
    let result = agent.run(resume).await.unwrap();
    assert_eq!(result.output, "done");
    assert_eq!(seen.lock().unwrap().len(), 1);
    let mut check = conversations.open("s").await.unwrap();
    assert_eq!(check.snapshot().await.unwrap().messages.len(), 2);
    check.finish(ConversationOutcome::Completed).await.unwrap();
    agent.close().await;
    conversations.close().await.unwrap();
    state.close().await.unwrap();
}
