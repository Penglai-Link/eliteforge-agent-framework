//! Black-box host contracts: these tests use only the published framework API.

use eliteforge_agent_test_support::*;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use eliteforge_agent_framework::*;
use serde_json::{Value, json};

#[derive(Clone)]
struct ScriptedModel {
    replies: Arc<Mutex<VecDeque<Result<ChatResponse, ModelError>>>>,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
}

impl ScriptedModel {
    fn new(replies: impl IntoIterator<Item = Result<ChatResponse, ModelError>>) -> Self {
        Self {
            replies: Arc::new(Mutex::new(replies.into_iter().collect())),
            requests: Arc::default(),
        }
    }

    fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl ChatModel for ScriptedModel {
    async fn generate(
        &self,
        _model_id: &str,
        request: ChatRequest,
        stream: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        self.requests.lock().unwrap().push(request);
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("the framework made an unexpected model request");
        if let Some(response) = reply
            .as_ref()
            .ok()
            .filter(|response| !response.content.is_empty())
        {
            stream.text_delta(&response.content.text()).await;
        }
        reply
    }
}

#[derive(Clone)]
struct CountingTool {
    calls: Arc<AtomicUsize>,
    result: Result<ToolOutput, ToolError>,
}

impl CountingTool {
    fn returning(output: ToolOutput) -> Self {
        Self {
            calls: Arc::default(),
            result: Ok(output),
        }
    }

    fn count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Tool for CountingTool {
    async fn call(&self, _: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.result.clone()
    }
}

fn binding(id: &str, name: &str, tool: CountingTool) -> ToolBinding {
    ToolBinding::new(
        ToolDescriptor::new(
            id,
            name,
            "Host-defined test capability",
            json!({"type": "object", "properties": {}}),
        ),
        tool,
    )
}

fn call(id: &str, name: &str) -> Result<ChatResponse, ModelError> {
    Ok(ChatResponse::tool_calls(vec![ToolCall::new(
        id,
        name,
        json!({}),
    )]))
}

fn text(value: &str) -> Result<ChatResponse, ModelError> {
    Ok(ChatResponse::text(value))
}

#[derive(Clone, Default)]
struct CollectedEvents(Arc<Mutex<Vec<AgentEvent>>>);

#[async_trait]
impl EventSink for CollectedEvents {
    async fn on_event(&self, _: &RunContext, event: &AgentEvent) {
        self.0.lock().unwrap().push(event.clone());
    }
}

struct HostContext;
struct HostIdentity(&'static str);

#[async_trait]
impl ContextProvider for HostContext {
    async fn context(&self, context: &RunContext) -> Result<Vec<Message>, AgentError> {
        let host = context.extensions.get::<HostIdentity>().unwrap();
        Ok(vec![Message::system(format!(
            "host instruction: {}",
            host.0
        ))])
    }
}

#[tokio::test]
async fn host_plugins_complete_model_tool_model_flow_and_preserve_both_tool_identifiers() {
    let mut final_response = ChatResponse::text("the app is healthy");
    final_response.usage = Usage {
        input_tokens: 17,
        output_tokens: 6,
    };
    let model = ScriptedModel::new([call("call-1", "app_page"), Ok(final_response)]);
    let tool = CountingTool::returning(ToolOutput::text("healthy"));
    let events = CollectedEvents::default();
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        .plugin(ToolProviderPlugin::new(
            "host.tools",
            StaticToolProvider::new(vec![binding("app.page", "app_page", tool.clone())]),
        ))
        .plugin(ContextProviderPlugin::new("host.context", HostContext))
        .plugin(EventSinkPlugin::new("host.events", events.clone()))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("conversation", "inspect the app");
    request.extensions.insert(HostIdentity("test host"));

    let result = agent.run(request).await.unwrap();

    assert_eq!(result.output, "the app is healthy");
    assert_eq!(result.status, RunStatus::Completed);
    assert_eq!(result.steps, 2);
    assert_eq!(result.tool_calls, 1);
    assert_eq!(result.usage.input_tokens, 17);
    assert_eq!(result.usage.output_tokens, 6);
    assert_eq!(tool.count(), 1);
    let requests = model.requests();
    assert!(requests[0].messages.iter().any(|message| {
        message.role == Role::System && message.content.text() == "host instruction: test host"
    }));
    assert_eq!(requests[0].tools[0].id, "app.page");
    assert_eq!(requests[0].tools[0].call_name, "app_page");
    assert!(requests[1].messages.iter().any(|message| {
        message.role == Role::Tool
            && message.tool_call_id.as_deref() == Some("call-1")
            && message.content.text().contains("healthy")
    }));
    let events = events.0.lock().unwrap();
    assert!(matches!(
        events.first(),
        Some(AgentEvent::RunStarted { .. })
    ));
    assert!(events.iter().any(|event| matches!(event,
        AgentEvent::ToolStarted { tool_id, call_id }
            if tool_id == "app.page" && call_id == "call-1"
    )));
    assert!(events.iter().any(|event| matches!(event,
        AgentEvent::TextDelta(delta) if delta == "the app is healthy"
    )));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::RunFinished {
            status: RunStatus::Completed
        })
    ));
}

#[tokio::test]
async fn default_memory_remembers_one_session_without_leaking_into_another() {
    let model = ScriptedModel::new([
        text("remembered alpha"),
        text("alpha reply"),
        text("beta reply"),
    ]);
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        .build()
        .await
        .unwrap();

    agent
        .run(RunRequest::new("alpha", "alpha private fact"))
        .await
        .unwrap();
    agent
        .run(RunRequest::new("alpha", "recall it"))
        .await
        .unwrap();
    agent
        .run(RunRequest::new("beta", "independent request"))
        .await
        .unwrap();

    let requests = model.requests();
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.content.text() == "alpha private fact")
    );
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.content.text() == "remembered alpha")
    );
    assert!(
        requests[2]
            .messages
            .iter()
            .all(|message| !message.content.text().contains("alpha"))
    );
    assert!(
        requests[2]
            .messages
            .iter()
            .any(|message| message.content.text() == "independent request")
    );
}

#[tokio::test]
async fn differently_composed_agents_isolate_models_tools_and_memory_for_the_same_session_id() {
    let alpha_model = ScriptedModel::new([
        call("lookup-call", "lookup"),
        text("alpha answer"),
        text("alpha recalled"),
    ]);
    let beta_model = ScriptedModel::new([
        call("lookup-call", "lookup"),
        text("beta answer"),
        text("beta recalled"),
    ]);
    let alpha_tool = CountingTool::returning(ToolOutput::text("alpha tool data"));
    let beta_tool = CountingTool::returning(ToolOutput::text("beta tool data"));
    let beta_events = CollectedEvents::default();
    let alpha = TestBuilder::default()
        .plugin(ChatModelPlugin::new(alpha_model.clone()))
        .plugin(ToolProviderPlugin::new(
            "host.tools",
            StaticToolProvider::new(vec![binding("alpha.lookup", "lookup", alpha_tool.clone())]),
        ))
        .plugin(ContextProviderPlugin::new("alpha.context", HostContext))
        .build()
        .await
        .unwrap();
    let beta = TestBuilder::default()
        .plugin(ChatModelPlugin::new(beta_model.clone()))
        .plugin(ToolProviderPlugin::new(
            "host.tools",
            StaticToolProvider::new(vec![binding("beta.lookup", "lookup", beta_tool.clone())]),
        ))
        .plugin(EventSinkPlugin::new("beta.events", beta_events.clone()))
        .build()
        .await
        .unwrap();
    let alpha_request = |input: &str| {
        let mut request = RunRequest::new("same-session", input);
        request.extensions.insert(HostIdentity("alpha context"));
        request
    };

    let alpha_first = alpha
        .run(alpha_request("alpha private question"))
        .await
        .unwrap();
    let beta_first = beta
        .run(RunRequest::new("same-session", "beta private question"))
        .await
        .unwrap();
    let alpha_second = alpha.run(alpha_request("recall")).await.unwrap();
    let beta_second = beta
        .run(RunRequest::new("same-session", "recall"))
        .await
        .unwrap();

    assert_eq!(alpha_first.output, "alpha answer");
    assert_eq!(beta_first.output, "beta answer");
    assert_eq!(alpha_second.output, "alpha recalled");
    assert_eq!(beta_second.output, "beta recalled");
    assert_eq!(alpha_tool.count(), 1);
    assert_eq!(beta_tool.count(), 1);
    for (model, own, other) in [
        (&alpha_model, "alpha", "beta"),
        (&beta_model, "beta", "alpha"),
    ] {
        let requests = model.requests();
        assert_eq!(requests.len(), 3);
        for request in &requests {
            assert_eq!(request.tools.len(), 1);
            assert_eq!(request.tools[0].id, format!("{own}.lookup"));
            assert_eq!(request.tools[0].call_name, "lookup");
            assert!(
                request
                    .messages
                    .iter()
                    .all(|message| !message.content.text().contains(other))
            );
        }
        for retained in ["private question", "tool data", "answer"] {
            assert!(
                requests[2]
                    .messages
                    .iter()
                    .any(|message| message.content.text() == format!("{own} {retained}"))
            );
        }
    }
    let events = beta_events.0.lock().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::RunStarted { .. }))
            .count(),
        2
    );
    assert!(events.iter().all(|event| match event {
        AgentEvent::ToolStarted { tool_id, .. } => tool_id == "beta.lookup",
        AgentEvent::TextDelta(delta) => !delta.contains("alpha"),
        _ => true,
    }));
}

#[derive(Clone, Default)]
struct HostMemory {
    opened: Arc<Mutex<Vec<String>>>,
    writes: Arc<Mutex<Vec<Message>>>,
    closed: Arc<AtomicUsize>,
}

struct HostMemorySession {
    messages: Vec<Message>,
    writes: Arc<Mutex<Vec<Message>>>,
    closed: Arc<AtomicUsize>,
}

#[async_trait]
impl ConversationStore for HostMemory {
    async fn open(
        &self,
        session_id: &str,
    ) -> Result<Box<dyn ConversationSession>, ConversationError> {
        self.opened.lock().unwrap().push(session_id.into());
        Ok(Box::new(HostMemorySession {
            messages: vec![Message::assistant("loaded from the host store")],
            writes: self.writes.clone(),
            closed: self.closed.clone(),
        }))
    }
}

#[async_trait]
impl ConversationSession for HostMemorySession {
    async fn snapshot(&mut self) -> Result<ConversationSnapshot, ConversationError> {
        Ok(ConversationSnapshot::new(
            ConversationKey::new("host"),
            "fixture",
            self.messages.clone(),
        ))
    }

    async fn append(&mut self, messages: &[Message]) -> Result<(), ConversationError> {
        self.messages.extend_from_slice(messages);
        self.writes.lock().unwrap().extend_from_slice(messages);
        Ok(())
    }

    async fn finish(&mut self, _: ConversationOutcome) -> Result<(), ConversationError> {
        self.closed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn memory_plugin_can_replace_the_default_without_changing_the_agent_loop() {
    let memory = HostMemory::default();
    let model = ScriptedModel::new([text("new answer")]);
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        .plugin(ConversationStorePlugin::new(memory.clone()))
        .build()
        .await
        .unwrap();

    agent
        .run(RunRequest::new("host-session-id", "new question"))
        .await
        .unwrap();

    assert_eq!(*memory.opened.lock().unwrap(), ["host-session-id"]);
    assert_eq!(memory.closed.load(Ordering::SeqCst), 1);
    assert!(
        model.requests()[0]
            .messages
            .iter()
            .any(|message| message.content.text() == "loaded from the host store")
    );
    let writes = memory.writes.lock().unwrap();
    assert!(
        writes
            .iter()
            .any(|message| message.role == Role::User && message.content.text() == "new question")
    );
    assert!(
        writes.iter().any(
            |message| message.role == Role::Assistant && message.content.text() == "new answer"
        )
    );
}

struct EchoLoop;

#[async_trait]
impl AgentLoop for EchoLoop {
    async fn run(
        &self,
        request: &RunRequest,
        _: &RunContext,
        _: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        Ok(RunResult {
            interactions: Vec::new(),
            content: Content::default(),
            structured: None,
            output: format!("custom loop: {}", request.input),
            status: RunStatus::Completed,
            usage: Usage::default(),
            steps: 0,
            tool_calls: 0,
        })
    }
}

#[tokio::test]
async fn a_custom_loop_runs_without_a_model_dependency() {
    let agent = TestBuilder::default()
        .plugin(AgentLoopPlugin::new(EchoLoop))
        .build()
        .await
        .unwrap();

    let result = agent
        .run(RunRequest::new("session", "hello"))
        .await
        .unwrap();

    assert_eq!(result.output, "custom loop: hello");
    assert_eq!(result.steps, 0);
}

#[tokio::test]
async fn a_tool_error_becomes_an_observation_that_the_model_can_recover_from() {
    let tool = CountingTool {
        calls: Arc::default(),
        result: Err(ToolError::Failed("host service unavailable".into())),
    };
    let model = ScriptedModel::new([
        call("failed-call", "fetch"),
        text("service unavailable; try later"),
    ]);
    let events = CollectedEvents::default();
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        .plugin(ToolProviderPlugin::new(
            "host.tools",
            StaticToolProvider::new(vec![binding("data.fetch", "fetch", tool.clone())]),
        ))
        .plugin(EventSinkPlugin::new("host.events", events.clone()))
        .build()
        .await
        .unwrap();

    let result = agent
        .run(RunRequest::new("session", "fetch data"))
        .await
        .unwrap();

    assert_eq!(result.status, RunStatus::Completed);
    assert_eq!(tool.count(), 1);
    assert!(model.requests()[1].messages.iter().any(|message| {
        message.role == Role::Tool
            && message.tool_call_id.as_deref() == Some("failed-call")
            && message.content.text().contains("host service unavailable")
    }));
    assert!(events.0.lock().unwrap().iter().any(|event| matches!(event,
        AgentEvent::ToolFinished { tool_id, is_error: true, .. } if tool_id == "data.fetch"
    )));
}

#[tokio::test]
async fn hidden_and_unknown_tools_cannot_be_invoked_by_guessing_names() {
    let hidden = CountingTool::returning(ToolOutput::text("private"));
    let model = ScriptedModel::new([
        call("hidden", "hidden_tool"),
        call("unknown", "not_authorized"),
        text("no access"),
    ]);
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        .plugin(ToolProviderPlugin::new(
            "host.tools",
            StaticToolProvider::new(vec![
                binding("private.read", "hidden_tool", hidden.clone()).hidden(),
            ]),
        ))
        .build()
        .await
        .unwrap();

    let result = agent
        .run(RunRequest::new("session", "read secrets"))
        .await
        .unwrap();

    assert_eq!(result.status, RunStatus::Completed);
    assert_eq!(hidden.count(), 0);
    assert!(
        model
            .requests()
            .iter()
            .all(|request| request.tools.is_empty())
    );
    let requests = model.requests();
    for call_id in ["hidden", "unknown"] {
        assert!(requests.last().unwrap().messages.iter().any(|message| {
            message.role == Role::Tool && message.tool_call_id.as_deref() == Some(call_id)
        }));
    }
}

#[tokio::test]
async fn discovery_can_expose_an_authorized_hidden_tool_by_its_stable_id() {
    let discovery = CountingTool::returning(
        ToolOutput::text("available").expose_tools(vec!["app.page".into()]),
    );
    let app = CountingTool::returning(ToolOutput::text("app data"));
    let model = ScriptedModel::new([
        call("discover", "discover"),
        call("app", "app_page"),
        text("done"),
    ]);
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        .plugin(ToolProviderPlugin::new(
            "host.tools",
            StaticToolProvider::new(vec![
                binding("tools.discover", "discover", discovery),
                binding("app.page", "app_page", app.clone()).hidden(),
            ]),
        ))
        .build()
        .await
        .unwrap();

    agent
        .run(RunRequest::new("session", "inspect app"))
        .await
        .unwrap();

    let requests = model.requests();
    assert_eq!(requests[0].tools.len(), 1);
    assert!(
        requests[1]
            .tools
            .iter()
            .any(|tool| tool.id == "app.page" && tool.call_name == "app_page")
    );
    assert_eq!(app.count(), 1);
}

#[tokio::test]
async fn tool_identity_or_call_name_conflicts_are_rejected_before_model_execution() {
    for (first_id, first_name, second_id, second_name) in [
        ("catalog.one", "same_name", "catalog.two", "same_name"),
        ("same.id", "first_name", "same.id", "second_name"),
    ] {
        let tool = CountingTool::returning(ToolOutput::text("unreachable"));
        let model = ScriptedModel::new([]);
        let agent = TestBuilder::default()
            .plugin(ChatModelPlugin::new(model.clone()))
            .plugin(ToolProviderPlugin::new(
                "first.provider",
                StaticToolProvider::new(vec![binding(first_id, first_name, tool.clone())]),
            ))
            .plugin(ToolProviderPlugin::new(
                "second.provider",
                StaticToolProvider::new(vec![binding(second_id, second_name, tool.clone())]),
            ))
            .build()
            .await;
        match agent {
            Ok(agent) => {
                let result = agent.run(RunRequest::new("session", "run")).await;
                assert!(matches!(result, Err(AgentError::ToolSetup(_))));
            }
            Err(error) => assert!(matches!(error, AgentError::ToolSetup(_))),
        }
        assert!(model.requests().is_empty());
        assert_eq!(tool.count(), 0);
    }
}

#[tokio::test]
async fn discovery_rejects_unknown_ids_without_partially_exposing_other_tools() {
    let discovery = CountingTool::returning(
        ToolOutput::text("attempted discovery")
            .expose_tools(vec!["private.read".into(), "unknown.admin".into()]),
    );
    let private = CountingTool::returning(ToolOutput::text("private data"));
    let model = ScriptedModel::new([
        call("discover", "discover"),
        call("unknown", "unknown_admin"),
        call("hidden", "private_read"),
        text("no access"),
    ]);
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        .plugin(ToolProviderPlugin::new(
            "host.tools",
            StaticToolProvider::new(vec![
                binding("tools.discover", "discover", discovery.clone()),
                binding("private.read", "private_read", private.clone()).hidden(),
            ]),
        ))
        .build()
        .await
        .unwrap();

    let result = agent
        .run(RunRequest::new("session", "discover restricted tools"))
        .await
        .unwrap();

    assert_eq!(result.status, RunStatus::Completed);
    assert_eq!(discovery.count(), 1);
    assert_eq!(private.count(), 0);
    for request in model.requests() {
        assert_eq!(request.tools.len(), 1);
        assert_eq!(request.tools[0].id, "tools.discover");
        assert_eq!(request.tools[0].call_name, "discover");
    }
}

#[tokio::test]
async fn configured_step_and_tool_budgets_bound_side_effects() {
    let tool = CountingTool::returning(ToolOutput::text("side effect completed"));
    let model = ScriptedModel::new([call("first", "act")]);
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        .plugin(ToolProviderPlugin::new(
            "host.tools",
            StaticToolProvider::new(vec![binding("action.execute", "act", tool.clone())]),
        ))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("step-limited", "act repeatedly");
    request.limits.max_steps = 1;

    let result = agent.run(request).await.unwrap();

    assert_eq!(result.status, RunStatus::StepLimit);
    assert_eq!(model.requests().len(), 1);
    assert_eq!(tool.count(), 1);

    let tool = CountingTool::returning(ToolOutput::text("side effect completed"));
    let model = ScriptedModel::new([Ok(ChatResponse::tool_calls(vec![
        ToolCall::new("first", "act", json!({})),
        ToolCall::new("second", "act", json!({})),
    ]))]);
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model))
        .plugin(ToolProviderPlugin::new(
            "host.tools",
            StaticToolProvider::new(vec![binding("action.execute", "act", tool.clone())]),
        ))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("tool-limited", "act twice");
    request.limits.max_tool_calls = 1;

    let result = agent.run(request).await.unwrap();

    assert_eq!(result.status, RunStatus::ToolCallLimit);
    assert!(tool.count() <= 1);
    assert!(result.tool_calls <= 1);
}

#[tokio::test]
async fn retrying_the_model_after_a_successful_tool_does_not_replay_the_tool() {
    let tool = CountingTool::returning(ToolOutput::text("payment recorded once"));
    let model = ScriptedModel::new([
        call("payment", "record_payment"),
        Err(ModelError::Transient("temporary transport failure".into())),
        text("payment complete"),
    ]);
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        .plugin(ToolProviderPlugin::new(
            "host.tools",
            StaticToolProvider::new(vec![binding(
                "payment.record",
                "record_payment",
                tool.clone(),
            )]),
        ))
        .build()
        .await
        .unwrap();

    let result = agent
        .run(RunRequest::new("session", "record payment"))
        .await
        .unwrap();

    assert_eq!(result.output, "payment complete");
    assert_eq!(tool.count(), 1);
    assert_eq!(result.tool_calls, 1);
    let requests = model.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1].messages, requests[2].messages);
}

#[tokio::test]
async fn asking_for_input_pauses_before_later_side_effects_in_the_same_batch() {
    let ask = CountingTool::returning(ToolOutput::text("Which environment?").await_input());
    let act = CountingTool::returning(ToolOutput::text("deployed"));
    let model = ScriptedModel::new([
        Ok(ChatResponse::tool_calls(vec![
            ToolCall::new("question", "ask", json!({})),
            ToolCall::new("deployment", "deploy", json!({})),
        ])),
        text("clarification received"),
    ]);
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        .plugin(ToolProviderPlugin::new(
            "host.tools",
            StaticToolProvider::new(vec![
                binding("user.ask", "ask", ask.clone()),
                binding("app.deploy", "deploy", act.clone()),
            ]),
        ))
        .build()
        .await
        .unwrap();

    let result = agent
        .run(RunRequest::new("session", "deploy app"))
        .await
        .unwrap();

    assert_eq!(result.status, RunStatus::AwaitingInput);
    assert_eq!(ask.count(), 1);
    assert_eq!(act.count(), 0);
    assert_eq!(model.requests().len(), 1);

    let response = InteractionManager(
        agent
            .service::<InteractionStoreService>()
            .unwrap()
            .0
            .clone(),
    )
    .answer("session", result.interactions[0].id, json!("use staging"))
    .await
    .unwrap();
    agent.run(response).await.unwrap();
    let requests = model.requests();
    let resumed_messages = &requests[1].messages;
    for tool_call in resumed_messages
        .iter()
        .flat_map(|message| &message.tool_calls)
    {
        assert!(resumed_messages.iter().any(|message| {
            message.role == Role::Tool
                && message.tool_call_id.as_deref() == Some(tool_call.id.as_str())
        }));
    }
    assert_eq!(act.count(), 0);
}

struct LeasedTools {
    released: Arc<AtomicUsize>,
}

#[async_trait]
impl ToolProvider for LeasedTools {
    async fn lease(&self, _: &RunContext) -> Result<ToolLease, AgentError> {
        let released = self.released.clone();
        Ok(ToolLease::new(vec![]).with_cleanup(move || {
            released.fetch_add(1, Ordering::SeqCst);
        }))
    }
}

#[tokio::test]
async fn run_local_resources_are_released_when_the_model_fails() {
    let released = Arc::new(AtomicUsize::new(0));
    let model = ScriptedModel::new([Err(ModelError::Fatal("invalid model configuration".into()))]);
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model))
        .plugin(ToolProviderPlugin::new(
            "host.leased-tools",
            LeasedTools {
                released: released.clone(),
            },
        ))
        .build()
        .await
        .unwrap();

    let result = agent.run(RunRequest::new("session", "run")).await;

    assert!(matches!(
        result,
        Err(AgentError::Model(ModelError::Fatal(_)))
    ));
    assert_eq!(released.load(Ordering::SeqCst), 1);
}
