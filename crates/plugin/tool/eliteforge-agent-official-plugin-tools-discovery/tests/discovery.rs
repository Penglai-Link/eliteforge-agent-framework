use eliteforge_agent_official_plugin_tools_discovery::ToolDiscoveryPlugin;
use eliteforge_agent_test_support::*;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
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

/// An async tool that awaits across a delay before finishing, tracking the
/// number of concurrently running instances.
#[derive(Clone)]
struct AsyncTool {
    calls: Arc<AtomicUsize>,
    active: Arc<AtomicUsize>,
    maximum: Arc<AtomicUsize>,
    delay: Duration,
}

#[async_trait]
impl Tool for AsyncTool {
    async fn call(&self, _: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let entered = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.maximum.fetch_max(entered, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(ToolOutput::text("async done"))
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

fn envelope(id: &str, tool: &str, arguments: Value) -> Result<ChatResponse, ModelError> {
    Ok(ChatResponse::tool_calls(vec![ToolCall::new(
        id,
        "tool_execute",
        json!({"tool": tool, "arguments": {"items":[arguments]}}),
    )]))
}

fn text(value: &str) -> Result<ChatResponse, ModelError> {
    Ok(ChatResponse::text(value))
}

fn search(query: &str) -> Result<ChatResponse, ModelError> {
    Ok(ChatResponse::tool_calls(vec![ToolCall::new(
        "find",
        "tool_search",
        json!({"items":[{"query":query}]}),
    )]))
}

/// The cache-stability invariant: every model request carries exactly the two
/// stable gateway descriptors for the whole run.
fn assert_stable_gateway_catalog(requests: &[ChatRequest]) {
    for (index, request) in requests.iter().enumerate() {
        let names: Vec<_> = request.tools.iter().map(|t| t.call_name.as_str()).collect();
        assert_eq!(
            names,
            vec!["tool_execute", "tool_search"],
            "request {index} must keep the stable gateway catalog"
        );
    }
}

#[tokio::test]
async fn gateway_keeps_two_stable_tools_and_executes_envelopes() {
    let app = CountingTool::returning(ToolOutput::text("app data"));
    let private = CountingTool::returning(ToolOutput::text("private"));
    let model = ScriptedModel::new([
        // A direct call to a wrapped name cannot pass admission.
        call("guess", "app_page"),
        search("app.page"),
        envelope("app", "app_page", json!({})),
        text("done"),
        // A second run sees the same two tools and keeps executing.
        envelope("again", "app_page", json!({})),
        text("done again"),
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
    assert_stable_gateway_catalog(&requests);
    // The direct guess is rejected with an error result for the model.
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.content.text().contains("not exposed")),
        "the direct call must be rejected"
    );
    // Broad search returns compact summaries as plain tool output.
    assert!(
        requests[2]
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.content.text().contains("app_page")),
        "search output must include the tool summary"
    );
    assert_eq!(app.count(), 1);
    agent
        .run(RunRequest::new("two", "try again"))
        .await
        .unwrap();
    assert_stable_gateway_catalog(&model.requests());
    assert_eq!(app.count(), 2);
    assert_eq!(private.count(), 0);
    assert!(
        model
            .requests()
            .iter()
            .all(|r| r.tools.iter().all(|t| t.id != "private.read"))
    );
    // Wrapped tools never re-enter the model-visible list even mid-run.
    assert!(
        model
            .requests()
            .iter()
            .all(|r| r.tools.iter().all(|t| t.id != "app.page"))
    );
    assert!(agent.framework().close().await.is_success());
}

#[tokio::test]
async fn collision_with_gateway_names_is_rejected_before_the_model() {
    for name in ["tool_search", "tool_execute"] {
        let model = ScriptedModel::new([]);
        let tool = CountingTool::returning(ToolOutput::text("unused"));
        let agent = TestBuilder::default()
            .plugin(ChatModelPlugin::new(model.clone()))
            .plugin(
                ToolDiscoveryPlugin::new().provider(StaticToolProvider::new(vec![binding(
                    "bad",
                    name,
                    tool.clone(),
                )])),
            )
            .build()
            .await
            .unwrap();
        assert!(
            matches!(
                agent.run(RunRequest::new("session", "go")).await,
                Err(AgentError::ToolSetup(_))
            ),
            "{name} must collide"
        );
        assert!(model.requests().is_empty());
        assert_eq!(tool.count(), 0);
        assert!(agent.framework().close().await.is_success());
    }
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
async fn enveloped_calls_validate_inner_schema_and_evaluate_policy_on_the_inner_tool() {
    let tool = CountingTool::returning(ToolOutput::text("must not run"));
    let evaluated = Arc::new(AtomicUsize::new(0));
    let closed = Arc::new(AtomicUsize::new(0));
    let model = ScriptedModel::new([
        search("app.page"),
        // Arguments violate the inner schema (missing required "path").
        envelope("bad-schema", "app_page", json!({"nope": 1})),
        // Policy denies the inner tool after a schema-valid envelope.
        envelope("denied", "app_page", json!({"path":"test"})),
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
    // Schema validation happens before policy evaluation, so only the valid
    // envelope reaches the policy chain, which observes the inner descriptor.
    assert_eq!(evaluated.load(Ordering::SeqCst), 1);
    assert_eq!(closed.load(Ordering::SeqCst), 1);
    let requests = model.requests();
    let last = requests.last().unwrap();
    assert!(
        last.messages
            .iter()
            .any(|m| m.role == Role::Tool && m.content.text().contains("do not match input schema")),
        "the schema violation must be reported"
    );
    assert!(
        last.messages
            .iter()
            .any(|m| m.role == Role::Tool && m.content.text().contains("host denied")),
        "the policy denial must be reported"
    );
    assert_stable_gateway_catalog(&requests);
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
async fn revoked_inner_tools_are_skipped_by_search_and_rejected_at_admission() {
    let tool = CountingTool::returning(ToolOutput::text("must not run"));
    let model = ScriptedModel::new([
        search("app.page"),
        envelope("guessed", "app_page", json!({})),
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
    // Search omits the revoked tool from its output.
    assert!(
        !model.requests()[1]
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.content.text().contains("app_page"))
    );
    assert!(
        model
            .requests()
            .iter()
            .all(|r| r.tools.iter().all(|t| t.id != "app.page"))
    );
    assert_stable_gateway_catalog(&model.requests());
    assert!(agent.framework().close().await.is_success());
}

#[tokio::test]
async fn invalid_envelopes_fail_with_clear_errors() {
    let tool = CountingTool::returning(ToolOutput::text("must not run"));
    let model = ScriptedModel::new([
        // Missing the required "tool" field.
        Ok(ChatResponse::tool_calls(vec![ToolCall::new(
            "no-tool",
            "tool_execute",
            json!({"arguments": {}}),
        )])),
        // Unknown inner tool name.
        envelope("ghost", "no_such_tool", json!({})),
        text("done"),
    ]);
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
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
        .run(RunRequest::new("one", "break things"))
        .await
        .unwrap();
    assert_eq!(tool.count(), 0);
    let requests = model.requests();
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.content.text().contains("tool_execute")),
        "the envelope error must mention tool_execute"
    );
    assert!(
        requests[2]
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.content.text().contains("not exposed")),
        "the unknown inner tool must be rejected"
    );
    assert_stable_gateway_catalog(&requests);
    assert!(agent.framework().close().await.is_success());
}

#[tokio::test]
async fn gateway_executes_async_tools_in_parallel_batches() {
    // Shared across both async tools so the test observes true cross-tool overlap.
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let first = AsyncTool {
        calls: Arc::default(),
        active: active.clone(),
        maximum: maximum.clone(),
        delay: Duration::from_millis(80),
    };
    let second = AsyncTool {
        calls: Arc::default(),
        active: active.clone(),
        maximum: maximum.clone(),
        delay: Duration::from_millis(40),
    };
    let sync = CountingTool::returning(ToolOutput::text("sync done"));
    let model = ScriptedModel::new([
        // Two parallel-safe async tools in one batch, plus a sync tool later.
        Ok(ChatResponse::tool_calls(vec![
            ToolCall::new(
                "a",
                "tool_execute",
                json!({"tool":"slow_one","arguments":{"items":[{}]}}),
            ),
            ToolCall::new(
                "b",
                "tool_execute",
                json!({"tool":"slow_two","arguments":{"items":[{}]}}),
            ),
        ])),
        envelope("c", "sync_tool", json!({})),
        text("done"),
    ]);
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        .plugin(
            ToolDiscoveryPlugin::new().provider(StaticToolProvider::new(vec![
                ToolBinding::new(
                    ToolDescriptor::new(
                        "slow.one",
                        "slow_one",
                        "Long-running async capability",
                        json!({"type":"object"}),
                    ),
                    first.clone(),
                )
                .parallel_safe(),
                ToolBinding::new(
                    ToolDescriptor::new(
                        "slow.two",
                        "slow_two",
                        "Long-running async capability",
                        json!({"type":"object"}),
                    ),
                    second.clone(),
                )
                .parallel_safe(),
                binding("sync.tool", "sync_tool", sync.clone()),
            ])),
        )
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("parallel", "run things");
    request.limits.max_parallel_tools = 2;
    agent.run(request).await.unwrap();
    assert_eq!(first.calls.load(Ordering::SeqCst), 1);
    assert_eq!(second.calls.load(Ordering::SeqCst), 1);
    assert_eq!(sync.count(), 1);
    // The two async tools genuinely overlapped.
    assert_eq!(
        maximum.load(Ordering::SeqCst),
        2,
        "the parallel batch must overlap"
    );
    assert_eq!(active.load(Ordering::SeqCst), 0);
    assert_stable_gateway_catalog(&model.requests());
    assert!(agent.framework().close().await.is_success());
}

struct ApproveApp;
#[async_trait]
impl ToolPolicy for ApproveApp {
    async fn evaluate(
        &self,
        invocation: &ToolInvocationContext<'_>,
    ) -> Result<ToolPolicyDecision, AgentError> {
        if invocation.descriptor.id == "app.page" {
            Ok(ToolPolicyDecision::RequireApproval(
                "Allow this app call?".into(),
            ))
        } else {
            Ok(ToolPolicyDecision::Allow)
        }
    }
}

#[tokio::test]
async fn gateway_approval_gates_the_inner_tool() {
    let app = CountingTool::returning(ToolOutput::text("app data"));
    let model = ScriptedModel::new([envelope("app", "app_page", json!({})), text("done")]);
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        .plugin(ToolPolicyPlugin::new("host.approval", ApproveApp))
        .plugin(
            ToolDiscoveryPlugin::new().provider(StaticToolProvider::new(vec![binding(
                "app.page",
                "app_page",
                app.clone(),
            )])),
        )
        .build()
        .await
        .unwrap();
    let waiting = agent
        .run(RunRequest::new("approval", "use the app"))
        .await
        .unwrap();
    assert_eq!(waiting.status, RunStatus::AwaitingInput);
    assert_eq!(app.count(), 0);
    let interaction = waiting.interactions[0].clone();
    let resume = agent
        .answer("approval", interaction.id, json!(true))
        .await
        .unwrap();
    let result = agent.run(resume).await.unwrap();
    assert_eq!(result.output, "done");
    assert_eq!(app.count(), 1);
    assert_stable_gateway_catalog(&model.requests());
    assert!(agent.framework().close().await.is_success());
}

#[tokio::test]
async fn wrap_all_gateway_discovers_and_executes_other_providers_tools() {
    // The host installs the gateway with no explicit providers; every other
    // installed provider becomes wrapped, searchable inventory.
    let app = CountingTool::returning(ToolOutput::text("app data"));
    let model = ScriptedModel::new([
        search("app.page"),
        envelope("app", "app_page", json!({})),
        text("done"),
    ]);
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        .plugin(ToolDiscoveryPlugin::new())
        .plugin(ToolProviderPlugin::new(
            "host.tools",
            StaticToolProvider::new(vec![binding("app.page", "app_page", app.clone())]),
        ))
        .build()
        .await
        .unwrap();
    agent
        .run(RunRequest::new("wrap-all", "inspect app"))
        .await
        .unwrap();
    let requests = model.requests();
    assert_stable_gateway_catalog(&requests);
    // The search executor was re-minted with the complete leased inventory:
    // it finds the other provider's tool even though the gateway plugin
    // itself wraps no explicit providers.
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.content.text().contains("app_page")),
        "wrap-all search must cover other providers' tools"
    );
    assert_eq!(app.count(), 1);
    assert!(agent.framework().close().await.is_success());
}

#[tokio::test]
async fn multiple_gateway_plugins_coexist_behind_one_stable_pair() {
    // A host-wide gateway plus a capability pack that installs its own
    // gateway plugin under a different id must merge into a single pair
    // instead of colliding. (The framework rejects duplicate plugin ids, so
    // coexistence always happens across distinct plugin ids.)
    struct HostGateway;
    #[async_trait]
    impl eliteforge_agent_framework::Plugin for HostGateway {
        fn id(&self) -> &str {
            "host.tools.gateway"
        }
        async fn install(
            &self,
            context: &mut eliteforge_agent_framework::PluginContext<'_>,
        ) -> Result<(), eliteforge_agent_framework::PluginError> {
            eliteforge_agent_framework::ToolProviderPlugin::new(self.id(), HostGatewayProvider)
                .install(context)
                .await
        }
    }
    struct HostGatewayProvider;
    #[async_trait]
    impl eliteforge_agent_framework::ToolProvider for HostGatewayProvider {
        async fn lease(&self, _: &RunContext) -> Result<ToolLease, AgentError> {
            // Contributed with an empty inventory; the loop re-mints the
            // search binding from the complete leased catalog.
            Ok(ToolLease::new(vec![
                ToolBinding::new(search_descriptor(), GatewaySearch { entries: vec![] }),
                ToolBinding::new(execute_descriptor(), GatewayExecute),
            ]))
        }
    }
    let app = CountingTool::returning(ToolOutput::text("app data"));
    let remote = CountingTool::returning(ToolOutput::text("remote data"));
    // Both fixtures share the description "Host-defined test capability",
    // so a single AND query discovers both providers' tools.
    let model = ScriptedModel::new([
        search("host-defined"),
        envelope("app", "app_page", json!({})),
        envelope("remote", "remote_fetch", json!({})),
        text("done"),
    ]);
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        // Host-wide wrap-all gateway under its own plugin id.
        .plugin(HostGateway)
        // Pack-scoped gateway wrapping its own provider.
        .plugin(
            ToolDiscoveryPlugin::new().provider(StaticToolProvider::new(vec![binding(
                "remote.fetch",
                "remote_fetch",
                remote.clone(),
            )])),
        )
        .plugin(ToolProviderPlugin::new(
            "host.tools",
            StaticToolProvider::new(vec![binding("app.page", "app_page", app.clone())]),
        ))
        .build()
        .await
        .unwrap();
    agent
        .run(RunRequest::new("merged", "use both"))
        .await
        .unwrap();
    let requests = model.requests();
    assert_stable_gateway_catalog(&requests);
    // One search covers both the host provider and the pack's provider.
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.content.text().contains("app_page")),
        "search must cover the host provider"
    );
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.content.text().contains("remote_fetch")),
        "search must cover the pack provider"
    );
    assert_eq!(app.count(), 1);
    assert_eq!(remote.count(), 1);
    assert!(agent.framework().close().await.is_success());
}

fn domain_search(domain_code: &str) -> Result<ChatResponse, ModelError> {
    Ok(ChatResponse::tool_calls(vec![ToolCall::new(
        "find",
        "tool_search",
        json!({"items":[{"domainCode": domain_code}]}),
    )]))
}

/// A host groups the inventory without the framework knowing its taxonomy: the
/// run-scoped label map lands on bindings that carry no code of their own, and
/// that group is then listable by `domainCode` while an unlabeled group is not.
#[tokio::test]
async fn host_group_labels_make_plugin_tools_domain_searchable() {
    let app = CountingTool::returning(ToolOutput::text("app data"));
    let other = CountingTool::returning(ToolOutput::text("other data"));
    let model = ScriptedModel::new([
        // One run, three discovery steps: the host-labeled group, a group the
        // host never labeled, then the unchanged keyword path.
        domain_search("APP"),
        domain_search("absent"),
        search("app.page"),
        text("done"),
    ]);
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        .plugin(
            ToolDiscoveryPlugin::new().provider(StaticToolProvider::new(vec![
                binding("app.page", "app_page", app),
                binding("other.write", "other_write", other),
            ])),
        )
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("labels", "inspect app");
    request
        .extensions
        .insert(ToolGroupLabels::new().with("app.page", "app"));
    agent.run(request).await.unwrap();
    let requests = model.requests();
    assert_stable_gateway_catalog(&requests);
    assert_eq!(requests.len(), 4, "three discovery steps then the answer");
    // The labeled group lists its own tool only, matched case-insensitively.
    assert!(
        requests[1].messages.iter().any(|m| m.role == Role::Tool
            && m.content.text().contains("app_page")
            && !m.content.text().contains("other_write")),
        "the labeled group must list exactly its own tool: {:?}",
        requests[1].messages
    );
    // A group the host never labeled finds nothing instead of guessing.
    assert!(
        requests[2]
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.content.text().contains("\"matched\":0")),
        "an unlabeled group must find nothing: {:?}",
        requests[2].messages
    );
    // The keyword path still covers the same unlabeled binding.
    assert!(
        requests[3]
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.content.text().contains("app_page")),
        "keyword search must still cover the same tool"
    );
    assert!(agent.framework().close().await.is_success());
}

struct HidePrivate;
#[async_trait]
impl ToolPolicy for HidePrivate {
    async fn evaluate(
        &self,
        _: &ToolInvocationContext<'_>,
    ) -> Result<ToolPolicyDecision, AgentError> {
        Ok(ToolPolicyDecision::Allow)
    }
    async fn admit_binding(
        &self,
        binding: &ToolBinding,
        _: &RunContext,
    ) -> Result<ToolBindingAdmission, AgentError> {
        Ok(if binding.descriptor.call_name == "private_read" {
            ToolBindingAdmission::Hidden
        } else {
            ToolBindingAdmission::Allow
        })
    }
}

#[tokio::test]
async fn gateway_preserves_hidden_tools_until_activation_and_hides_them_on_replacement() {
    for policy_hidden in [false, true] {
        let private = CountingTool::returning(ToolOutput::text("private data"));
        let mut activate = ToolOutput::text("activated");
        activate.effect = ToolEffect::ExposeTools(vec!["private.read".into()]);
        let mut reset = ToolOutput::text("reset");
        reset.effect = ToolEffect::SetExposedTools(vec!["activate".into(), "reset".into()]);
        let model = ScriptedModel::new([
            search("private.read"),
            envelope("guess", "private_read", json!({})),
            envelope("activate", "activate", json!({})),
            search("private.read"),
            envelope("allowed", "private_read", json!({})),
            envelope("reset", "reset", json!({})),
            search("private.read"),
            envelope("hidden-again", "private_read", json!({})),
            text("done"),
        ]);
        let mut private_binding = binding("private.read", "private_read", private.clone());
        if !policy_hidden {
            private_binding = private_binding.hidden();
        }
        let mut builder = TestBuilder::default()
            .plugin(ChatModelPlugin::new(model.clone()))
            .plugin(
                ToolDiscoveryPlugin::new().provider(StaticToolProvider::new(vec![
                    private_binding,
                    binding("activate", "activate", CountingTool::returning(activate)),
                    binding("reset", "reset", CountingTool::returning(reset)),
                ])),
            );
        if policy_hidden {
            builder = builder.plugin(ToolPolicyPlugin::new("hide-private", HidePrivate));
        }
        let agent = builder.build().await.unwrap();
        let mut request = RunRequest::new("activation", "test hidden activation");
        request.limits.max_steps = 12;
        request.limits.max_tool_calls = 12;
        agent.run(request).await.unwrap();
        let requests = model.requests();
        assert_stable_gateway_catalog(&requests);
        for (step, matched) in [(1, 0), (4, 1), (7, 0)] {
            let output = requests[step]
                .messages
                .iter()
                .rev()
                .find(|m| m.role == Role::Tool)
                .unwrap();
            let data: Value = serde_json::from_str(&output.content.text()).unwrap();
            assert_eq!(
                data["results"][0]["result"]["matched"], matched,
                "step {step}, policy_hidden={policy_hidden}"
            );
        }
        for step in [2, 8] {
            assert!(
                requests[step]
                    .messages
                    .iter()
                    .rev()
                    .find(|m| m.role == Role::Tool)
                    .unwrap()
                    .content
                    .text()
                    .contains("not exposed")
            );
        }
        assert_eq!(private.count(), 1);
        assert!(agent.framework().close().await.is_success());
    }
}

#[tokio::test]
async fn duplicate_gateway_plugin_ids_are_rejected_at_assembly() {
    let result = TestBuilder::default()
        .plugin(ToolDiscoveryPlugin::new())
        .plugin(ToolDiscoveryPlugin::new())
        .build()
        .await;
    assert!(
        matches!(result, Err(AgentError::Plugin(PluginError::DuplicatePluginId { id }))
        if id == "framework.tools.discovery")
    );
}

#[tokio::test]
async fn gateway_activation_survives_approval_resume() {
    let app = CountingTool::returning(ToolOutput::text("app data"));
    let activate = CountingTool::returning(
        ToolOutput::text("activated").expose_tools(vec!["app.page".into()]),
    );
    let model = ScriptedModel::new([
        envelope("activate", "activate", json!({})),
        envelope("app", "app_page", json!({})),
        search("app.page"),
        text("done"),
    ]);
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        .plugin(ToolPolicyPlugin::new("host.approval", ApproveApp))
        .plugin(
            ToolDiscoveryPlugin::new().provider(StaticToolProvider::new(vec![
                binding("activate", "activate", activate),
                binding("app.page", "app_page", app.clone()).hidden(),
            ])),
        )
        .build()
        .await
        .unwrap();
    let waiting = agent
        .run(RunRequest::new("activation-resume", "activate then use"))
        .await
        .unwrap();
    assert_eq!(waiting.status, RunStatus::AwaitingInput);
    assert_eq!(app.count(), 0);
    let resume = agent
        .answer("activation-resume", waiting.interactions[0].id, json!(true))
        .await
        .unwrap();
    assert_eq!(agent.run(resume).await.unwrap().output, "done");
    assert_eq!(app.count(), 1);
    let requests = model.requests();
    assert_stable_gateway_catalog(&requests);
    let result = requests
        .last()
        .unwrap()
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Tool)
        .unwrap();
    let data: Value = serde_json::from_str(&result.content.text()).unwrap();
    assert_eq!(data["results"][0]["result"]["matched"], 1);
    assert!(agent.framework().close().await.is_success());
}

#[derive(Clone)]
struct BatchEcho(Arc<Mutex<Vec<i64>>>);
#[async_trait]
impl Tool for BatchEcho {
    async fn call(&self, input: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        let value = input["value"].as_i64().unwrap();
        self.0.lock().unwrap().push(value);
        if value == 3 {
            return Err(ToolError::Failed("item failed".into()));
        }
        let mut output = ToolOutput::text(value.to_string());
        output.structured = Some(json!({"value":value}));
        Ok(output)
    }
}
struct BatchPolicy;
#[async_trait]
impl ToolPolicy for BatchPolicy {
    async fn evaluate(
        &self,
        input: &ToolInvocationContext<'_>,
    ) -> Result<ToolPolicyDecision, AgentError> {
        assert_eq!(input.descriptor.id, "batch.echo");
        assert!(input.call.arguments.get("items").is_none());
        Ok(if input.call.arguments["value"] == 2 {
            ToolPolicyDecision::Deny("item denied".into())
        } else {
            ToolPolicyDecision::Allow
        })
    }
}
fn echo_binding(tool: BatchEcho) -> ToolBinding {
    ToolBinding::new(
        ToolDescriptor::new(
            "batch.echo",
            "echo",
            "Echo",
            json!({
                "type":"object","additionalProperties":false,"required":["value"],
                "properties":{"value":{"type":"integer"}}
            }),
        ),
        tool,
    )
}
fn batch_call(items: Value) -> Result<ChatResponse, ModelError> {
    Ok(ChatResponse::tool_calls(vec![ToolCall::new(
        "batch",
        TOOL_EXECUTE_CALL_NAME,
        json!({"tool":"echo","arguments":{"items":items}}),
    )]))
}

#[tokio::test]
async fn batch_items_keep_policy_schema_errors_order_and_provider_pairing() {
    let tool = BatchEcho(Arc::default());
    let model = ScriptedModel::new([
        batch_call(json!([{"value":1},{"value":2},{"value":"bad"},{"value":3},{"value":4}])),
        text("done"),
    ]);
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        .plugin(ToolPolicyPlugin::new("batch.policy", BatchPolicy))
        .plugin(
            ToolDiscoveryPlugin::new()
                .provider(StaticToolProvider::new(vec![echo_binding(tool.clone())])),
        )
        .build()
        .await
        .unwrap();
    let result = agent
        .run(RunRequest::new("batch", "batch requests"))
        .await
        .unwrap();
    assert_eq!(result.tool_calls, 5);
    assert_eq!(*tool.0.lock().unwrap(), vec![1, 3, 4]);
    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    let messages: Vec<_> = requests[1]
        .messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .collect();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].tool_call_id.as_deref(), Some("batch"));
    assert!(
        messages[0]
            .tool_result
            .as_ref()
            .unwrap()
            .structured
            .is_none(),
        "full audit data must not duplicate compact provider content"
    );
    let data: Value = serde_json::from_str(&messages[0].content.text()).unwrap();
    let rows = data["results"].as_array().unwrap();
    assert_eq!(rows.len(), 5);
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(row["index"], i);
    }
    assert_eq!(rows[0]["result"], json!({"value":1}));
    assert_eq!(rows[4]["result"], json!({"value":4}));
    assert_eq!(
        rows.iter()
            .map(|r| r["isError"].as_bool().unwrap())
            .collect::<Vec<_>>(),
        vec![false, true, true, true, false]
    );
    assert!(agent.framework().close().await.is_success());
}

#[tokio::test]
async fn batch_only_tools_reject_legacy_single_and_empty_or_oversized_batches() {
    let tool = BatchEcho(Arc::default());
    let legacy = Ok(ChatResponse::tool_calls(vec![ToolCall::new(
        "legacy",
        TOOL_EXECUTE_CALL_NAME,
        json!({"tool":"echo","arguments":{"value":1}}),
    )]));
    let model = ScriptedModel::new([
        legacy,
        batch_call(json!([])),
        batch_call(json!(vec![json!({"value":1}); 33])),
        text("done"),
    ]);
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        .plugin(
            ToolDiscoveryPlugin::new()
                .provider(StaticToolProvider::new(vec![echo_binding(tool.clone())])),
        )
        .build()
        .await
        .unwrap();
    agent
        .run(RunRequest::new("invalid-batch", "try"))
        .await
        .unwrap();
    assert!(tool.0.lock().unwrap().is_empty());
    assert!(
        model
            .requests()
            .last()
            .unwrap()
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .all(|m| m.tool_result.as_ref().unwrap().is_error)
    );
    assert!(agent.framework().close().await.is_success());
}

#[tokio::test]
async fn batch_budget_counts_items_and_emits_one_complete_provider_result() {
    let tool = BatchEcho(Arc::default());
    let model = ScriptedModel::new([batch_call(json!([{"value":1},{"value":2},{"value":4}]))]);
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        .plugin(
            ToolDiscoveryPlugin::new()
                .provider(StaticToolProvider::new(vec![echo_binding(tool.clone())])),
        )
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("budget-batch", "try");
    request.limits.max_tool_calls = 2;
    request.limits.tool_batch_budget_policy = ToolBatchBudgetPolicy::ExecuteAvailable;
    let result = agent.run(request).await.unwrap();
    assert_eq!(result.status, RunStatus::ToolCallLimit);
    assert_eq!(*tool.0.lock().unwrap(), vec![1, 2]);
    assert_eq!(result.tool_calls, 2);
    assert!(agent.framework().close().await.is_success());
}

#[tokio::test]
async fn batch_approval_resume_preserves_individual_decisions_and_audit() {
    let tool = CountingTool::returning(ToolOutput::text("done"));
    let model = ScriptedModel::new([
        Ok(ChatResponse::tool_calls(vec![ToolCall::new(
            "batch",
            TOOL_EXECUTE_CALL_NAME,
            json!({"tool":"app_page","arguments":{"items":[{},{}]}}),
        )])),
        text("finished"),
    ]);
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        .plugin(ToolPolicyPlugin::new("approval", ApproveApp))
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
    let waiting = agent
        .run(RunRequest::new("batch-approval", "try"))
        .await
        .unwrap();
    assert_eq!(waiting.interactions.len(), 2);
    let first = agent
        .answer("batch-approval", waiting.interactions[0].id, json!(true))
        .await
        .unwrap();
    let waiting = agent.run(first).await.unwrap();
    assert_eq!(tool.count(), 0, "no item may overtake unresolved approvals");
    let second = agent
        .answer("batch-approval", waiting.interactions[0].id, json!(false))
        .await
        .unwrap();
    let result = agent.run(second).await.unwrap();
    assert_eq!(result.status, RunStatus::Completed);
    assert_eq!(tool.count(), 1);
    let checkpoint = agent
        .framework()
        .services()
        .unwrap()
        .get::<StateStoreService>()
        .unwrap()
        .0
        .latest("batch-approval")
        .await
        .unwrap()
        .unwrap();
    let records = &checkpoint.history.tool_invocations;
    let parent = records.iter().find(|r| r.batch.is_none()).unwrap();
    let children: Vec<_> = records
        .iter()
        .filter(|r| {
            r.batch
                .as_ref()
                .is_some_and(|b| b.parent_invocation_id == parent.id)
        })
        .collect();
    assert_eq!(children.len(), 2);
    assert!(children.iter().all(|r| r.is_error.is_some()));
    assert_eq!(
        parent.structured.as_ref().unwrap()["results"][0]["isError"],
        false
    );
    assert_eq!(
        parent.structured.as_ref().unwrap()["results"][1]["isError"],
        true
    );
    assert_eq!(
        checkpoint
            .transcript
            .iter()
            .filter(|m| m.role == Role::Tool)
            .count(),
        1
    );
    assert!(agent.framework().close().await.is_success());
}

#[tokio::test]
async fn batch_discovery_returns_only_batch_contracts_and_explicit_single_exceptions() {
    let tool = CountingTool::returning(ToolOutput::text("ok"));
    let mut edit = binding("file.edit", "file_edit", tool.clone());
    edit.descriptor = edit.descriptor.with_single_input();
    let model = ScriptedModel::new([
        Ok(ChatResponse::tool_calls(vec![ToolCall::new(
            "search",
            TOOL_SEARCH_CALL_NAME,
            json!({"items":[{"tool":"app_read"},{"tool":"file_edit"}]}),
        )])),
        Ok(ChatResponse::tool_calls(vec![ToolCall::new(
            "edit",
            TOOL_EXECUTE_CALL_NAME,
            json!({"tool":"file_edit","arguments":{}}),
        )])),
        text("done"),
    ]);
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        .plugin(
            ToolDiscoveryPlugin::new().provider(StaticToolProvider::new(vec![
                binding("app.read", "app_read", tool.clone()),
                edit,
            ])),
        )
        .build()
        .await
        .unwrap();
    agent
        .run(RunRequest::new("contracts", "try"))
        .await
        .unwrap();
    let requests = model.requests();
    let search = requests[0]
        .tools
        .iter()
        .find(|t| t.call_name == TOOL_SEARCH_CALL_NAME)
        .unwrap();
    assert_eq!(search.input_schema["required"], json!(["items"]));
    let result: Value = serde_json::from_str(
        &requests[1]
            .messages
            .iter()
            .find(|m| m.role == Role::Tool)
            .unwrap()
            .content
            .text(),
    )
    .unwrap();
    assert_eq!(
        result["results"][0]["result"]["tools"][0]["input_schema"]["required"],
        json!(["items"])
    );
    assert!(
        result["results"][1]["result"]["tools"][0]["input_schema"]["properties"]
            .get("items")
            .is_none()
    );
    assert_eq!(tool.count(), 1);
    assert!(agent.framework().close().await.is_success());
}
