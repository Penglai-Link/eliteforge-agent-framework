use eliteforge_agent_official_plugin_tools_discovery::ToolDiscoveryPlugin;
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

fn search(query: &str) -> Result<ChatResponse, ModelError> {
    Ok(ChatResponse::tool_calls(vec![ToolCall::new(
        "find",
        "tool_search",
        json!({"query":query}),
    )]))
}

#[tokio::test]
async fn discovery_loads_schemas_then_uses_the_original_executor_and_resets_next_run() {
    let app = CountingTool::returning(ToolOutput::text("app data"));
    let private = CountingTool::returning(ToolOutput::text("private"));
    let model = ScriptedModel::new([
        call("guess", "app_page"),
        search("app.page"),
        call("app", "app_page"),
        text("done"),
        call("guess-again", "app_page"),
        search("private"),
        text("done"),
    ]);
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        .plugin(
            ToolDiscoveryPlugin::new().provider(StaticToolProvider::new(vec![
                binding("app.page", "app_page", app.clone()),
                binding("private.read", "private_read", private.clone()).hidden(),
            ])),
        )
        .build()
        .await
        .unwrap();
    agent
        .run(RunRequest::new("one", "inspect app"))
        .await
        .unwrap();
    let requests = model.requests();
    assert_eq!(requests[0].tools.len(), 1);
    assert_eq!(requests[0].tools[0].call_name, "tool_search");
    assert!(requests[2].tools.iter().any(|t| t.id == "app.page"));
    assert_eq!(app.count(), 1);
    agent
        .run(RunRequest::new("two", "try again"))
        .await
        .unwrap();
    assert_eq!(model.requests()[4].tools.len(), 1);
    assert_eq!(app.count(), 1);
    assert_eq!(private.count(), 0);
    assert!(
        model
            .requests()
            .iter()
            .all(|r| r.tools.iter().all(|t| t.id != "private.read"))
    );
    assert!(agent.framework().close().await.is_success());
}

#[tokio::test]
async fn collision_with_discovery_name_is_rejected_before_the_model() {
    let model = ScriptedModel::new([]);
    let tool = CountingTool::returning(ToolOutput::text("unused"));
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        .plugin(
            ToolDiscoveryPlugin::new().provider(StaticToolProvider::new(vec![binding(
                "bad",
                "tool_search",
                tool.clone(),
            )])),
        )
        .build()
        .await
        .unwrap();
    assert!(matches!(
        agent.run(RunRequest::new("session", "go")).await,
        Err(AgentError::ToolSetup(_))
    ));
    assert!(model.requests().is_empty());
    assert_eq!(tool.count(), 0);
    assert!(agent.framework().close().await.is_success());
}

struct DenyApp(Arc<AtomicUsize>);
#[async_trait]
impl ToolPolicy for DenyApp {
    async fn evaluate(
        &self,
        invocation: &ToolInvocationContext<'_>,
    ) -> Result<ToolPolicyDecision, AgentError> {
        if invocation.descriptor.id == "app.page" {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ToolPolicyDecision::Deny("host denied this app call".into()))
        } else {
            Ok(ToolPolicyDecision::Allow)
        }
    }
}

struct ScopedProvider {
    tool: CountingTool,
    closed: Arc<AtomicUsize>,
}
#[async_trait]
impl ToolProvider for ScopedProvider {
    async fn lease(&self, _: &RunContext) -> Result<ToolLease, AgentError> {
        let closed = self.closed.clone();
        Ok(ToolLease::new(vec![ToolBinding::new(ToolDescriptor::new("app.page", "app_page", "Read app", json!({"type":"object","required":["path"],"additionalProperties":false,"properties":{"path":{"type":"string"}}})), self.tool.clone())])
            .with_shutdown(move || async move { closed.fetch_add(1, Ordering::SeqCst); Ok(()) }))
    }
}

#[tokio::test]
async fn discovered_calls_still_validate_schema_evaluate_policy_and_close_leases() {
    let tool = CountingTool::returning(ToolOutput::text("must not run"));
    let evaluated = Arc::new(AtomicUsize::new(0));
    let closed = Arc::new(AtomicUsize::new(0));
    let model = ScriptedModel::new([
        search("app.page"),
        call("bad-schema", "app_page"),
        Ok(ChatResponse::tool_calls(vec![ToolCall::new(
            "denied",
            "app_page",
            json!({"path":"test"}),
        )])),
        text("done"),
    ]);
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        .plugin(ToolPolicyPlugin::new(
            "host.policy",
            DenyApp(evaluated.clone()),
        ))
        .plugin(ToolDiscoveryPlugin::new().provider(ScopedProvider {
            tool: tool.clone(),
            closed: closed.clone(),
        }))
        .build()
        .await
        .unwrap();
    agent
        .run(RunRequest::new("one", "inspect app"))
        .await
        .unwrap();
    assert_eq!(tool.count(), 0);
    assert_eq!(evaluated.load(Ordering::SeqCst), 1);
    assert_eq!(closed.load(Ordering::SeqCst), 1);
    let requests = model.requests();
    assert!(
        requests
            .last()
            .unwrap()
            .messages
            .iter()
            .any(|m| m.content.text().contains("host denied"))
    );
    assert!(agent.framework().close().await.is_success());
}

struct RevokeApp;
#[async_trait]
impl ToolPolicy for RevokeApp {
    async fn evaluate(
        &self,
        invocation: &ToolInvocationContext<'_>,
    ) -> Result<ToolPolicyDecision, AgentError> {
        invocation.run.revoke_tools(&["app.page".into()])?;
        Ok(ToolPolicyDecision::Allow)
    }
}

#[tokio::test]
async fn revoked_tools_are_not_returned_or_reactivated_by_search() {
    let tool = CountingTool::returning(ToolOutput::text("must not run"));
    let model = ScriptedModel::new([
        search("app.page"),
        call("guessed", "app_page"),
        text("done"),
    ]);
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        .plugin(ToolPolicyPlugin::new("host.revoke", RevokeApp))
        .plugin(
            ToolDiscoveryPlugin::new().provider(StaticToolProvider::new(vec![binding(
                "app.page",
                "app_page",
                tool.clone(),
            )])),
        )
        .build()
        .await
        .unwrap();
    agent
        .run(RunRequest::new("one", "inspect app"))
        .await
        .unwrap();
    assert_eq!(tool.count(), 0);
    assert!(
        model
            .requests()
            .iter()
            .all(|r| r.tools.iter().all(|t| t.id != "app.page"))
    );
    assert!(agent.framework().close().await.is_success());
}
