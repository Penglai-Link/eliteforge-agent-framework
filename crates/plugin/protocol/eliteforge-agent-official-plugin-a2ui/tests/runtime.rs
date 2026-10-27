use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_a2ui::*;
use eliteforge_agent_official_plugin_storage_local::LocalStoragePlugin;
use eliteforge_agent_official_plugin_tools_discovery::ToolDiscoveryPlugin;
use eliteforge_agent_test_support::fixture_selection;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct Model {
    replies: Arc<Mutex<std::collections::VecDeque<ChatResponse>>>,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
}
#[async_trait]
impl ChatModel for Model {
    async fn generate(
        &self,
        _: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        self.requests.lock().unwrap().push(request);
        Ok(self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected model call"))
    }
}
fn options() -> A2uiRunOptions {
    A2uiRunOptions {
        capabilities: A2uiClientCapabilities {
            version: VERSION.into(),
            catalog_ids: vec![AGENT_CATALOG_ID.into()],
        },
        owner_agent_id: "main-agent".into(),
    }
}
fn messages() -> Value {
    let examples: Value = serde_json::from_str(include_str!(
        "../skills/a2ui-interface/references/examples.json"
    ))
    .unwrap();
    examples["form"]["messages"].clone()
}

#[tokio::test]
async fn gateway_discovers_and_executes_a2ui_in_each_new_run() {
    let replies = ["first", "second"].into_iter().flat_map(|id| [
        ChatResponse::tool_calls(vec![ToolCall::new("search", "tool_search", json!({"items":[{"domainCode":"a2ui"}]}))]),
        ChatResponse::tool_calls(vec![ToolCall::new("publish", "tool_execute", json!({"tool":"a2ui_publish","arguments":{"mode":"Display","messages":[{"version":VERSION,"createSurface":{"surfaceId":id,"catalogId":AGENT_CATALOG_ID}},{"version":VERSION,"updateComponents":{"surfaceId":id,"components":[{"id":"root","component":"Text","text":"Hello"}]}}]}}))]),
        ChatResponse::text("done"),
    ]).collect();
    let model = Model {
        replies: Arc::new(Mutex::new(replies)),
        requests: Arc::default(),
    };
    let dir = tempfile::tempdir().unwrap();
    let artifacts = dir.path().join("artifacts");
    std::fs::create_dir(&artifacts).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&artifacts, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let framework = Framework::builder()
        .default_models(false)
        .plugin(LocalStoragePlugin::new(dir.path().join("s.db"), artifacts))
        .plugin(ReactPlugin)
        .plugin(ChatModelPlugin::new(model.clone()))
        .plugin(ToolDiscoveryPlugin::new())
        .capability_pack(A2uiPack::new().unwrap())
        .build()
        .await
        .unwrap();
    let session = framework
        .sessions()
        .unwrap()
        .create(SessionRecord::new("gateway"))
        .await
        .unwrap();
    let mut selection = fixture_selection(&framework).unwrap();
    selection.skills = Some(vec![A2UI_SKILL_NAME.into()]);
    let agent = framework.agent(selection).build().await.unwrap();
    for _ in 0..2 {
        let mut request = RunRequest::new(&session.id, "show a report");
        request.extensions.insert(options());
        assert_eq!(
            agent.run(request).await.unwrap().status,
            RunStatus::Completed
        );
    }
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 6);
    for request in requests.iter() {
        let names: Vec<_> = request.tools.iter().map(|t| t.call_name.as_str()).collect();
        assert_eq!(names, ["tool_execute", "tool_search"]);
    }
    for index in [1, 4] {
        let history = serde_json::to_string(&requests[index].messages).unwrap();
        assert!(history.contains("a2ui_publish") && history.contains("a2ui_request_input"));
        assert!(!history.contains("a2ui_expand"));
    }
    drop(requests);
    let state = framework
        .services()
        .unwrap()
        .get::<A2uiService>()
        .unwrap()
        .state(&session.id)
        .await
        .unwrap();
    assert_eq!(state.surfaces.len(), 2);
    assert!(framework.close().await.is_success());
}

#[tokio::test]
async fn gateway_form_suspends_and_resumes_without_activation() {
    let model = Model {
        replies: Arc::new(Mutex::new([
            ChatResponse::tool_calls(vec![ToolCall::new("search", "tool_search", json!({"items":[{"domainCode":"a2ui"}]}))]),
            ChatResponse::tool_calls(vec![ToolCall::new("form", "tool_execute", json!({"tool":"a2ui_request_input","arguments":{"mode":"SubmitOnce","messages":messages(),"responseSchema":{"type":"object","additionalProperties":false,"required":["name"],"properties":{"name":{"type":"string","minLength":1}}}}}))]),
            ChatResponse::text("done"),
        ].into())),
        requests: Arc::default(),
    };
    let dir = tempfile::tempdir().unwrap();
    let artifacts = dir.path().join("artifacts");
    std::fs::create_dir(&artifacts).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&artifacts, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let framework = Framework::builder()
        .default_models(false)
        .plugin(LocalStoragePlugin::new(dir.path().join("s.db"), artifacts))
        .plugin(ReactPlugin)
        .plugin(ChatModelPlugin::new(model.clone()))
        .plugin(ToolDiscoveryPlugin::new())
        .capability_pack(A2uiPack::new().unwrap())
        .build()
        .await
        .unwrap();
    let session = framework
        .sessions()
        .unwrap()
        .create(SessionRecord::new("gateway-form"))
        .await
        .unwrap();
    let mut selection = fixture_selection(&framework).unwrap();
    selection.skills = Some(vec![A2UI_SKILL_NAME.into()]);
    let agent = framework.agent(selection).build().await.unwrap();
    let mut request = RunRequest::new(&session.id, "ask my name");
    request.extensions.insert(options());
    let waiting = agent.run(request).await.unwrap();
    assert_eq!(waiting.status, RunStatus::AwaitingInput);
    assert_eq!(waiting.interactions.len(), 1);
    let mut resume = agent
        .answer(
            &session.id,
            waiting.interactions[0].id,
            json!({"name":"Alice"}),
        )
        .await
        .unwrap();
    resume.extensions.insert(options());
    assert_eq!(
        agent.run(resume).await.unwrap().status,
        RunStatus::Completed
    );
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(
        serde_json::to_string(&requests[2].messages)
            .unwrap()
            .contains("Alice")
    );
    assert!(requests.iter().all(|r| {
        r.tools
            .iter()
            .map(|t| t.call_name.as_str())
            .collect::<Vec<_>>()
            == ["tool_execute", "tool_search"]
    }));
    drop(requests);
    assert!(framework.close().await.is_success());
}

#[tokio::test]
async fn gateway_hides_and_rejects_a2ui_without_client_capability() {
    let model = Model {
        replies: Arc::new(Mutex::new(
            [
                ChatResponse::tool_calls(vec![ToolCall::new(
                    "search",
                    "tool_search",
                    json!({"items":[{"domainCode":"a2ui"}]}),
                )]),
                ChatResponse::tool_calls(vec![ToolCall::new(
                    "publish",
                    "tool_execute",
                    json!({"tool":"a2ui_publish","arguments":{"mode":"Display","messages":[]}}),
                )]),
                ChatResponse::text("done"),
            ]
            .into(),
        )),
        requests: Arc::default(),
    };
    let dir = tempfile::tempdir().unwrap();
    let artifacts = dir.path().join("artifacts");
    std::fs::create_dir(&artifacts).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&artifacts, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let framework = Framework::builder()
        .default_models(false)
        .plugin(LocalStoragePlugin::new(dir.path().join("s.db"), artifacts))
        .plugin(ReactPlugin)
        .plugin(ChatModelPlugin::new(model.clone()))
        .plugin(ToolDiscoveryPlugin::new())
        .capability_pack(A2uiPack::new().unwrap())
        .build()
        .await
        .unwrap();
    let session = framework
        .sessions()
        .unwrap()
        .create(SessionRecord::new("no a2ui"))
        .await
        .unwrap();
    let agent = framework
        .agent(fixture_selection(&framework).unwrap())
        .build()
        .await
        .unwrap();
    assert_eq!(
        agent
            .run(RunRequest::new(&session.id, "show a report"))
            .await
            .unwrap()
            .status,
        RunStatus::Completed
    );
    let requests = model.requests.lock().unwrap();
    let discovery = requests[1]
        .messages
        .iter()
        .find(|m| m.role == Role::Tool)
        .unwrap();
    assert!(!discovery.content.text().contains("a2ui_publish"));
    let rejected = requests[2]
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Tool)
        .unwrap();
    assert!(rejected.tool_result.as_ref().unwrap().is_error);
    drop(requests);
    assert!(
        framework
            .services()
            .unwrap()
            .get::<A2uiService>()
            .unwrap()
            .state(&session.id)
            .await
            .unwrap()
            .surfaces
            .is_empty()
    );
    assert!(framework.close().await.is_success());
}

struct DenyPublish;
#[async_trait]
impl ToolPolicy for DenyPublish {
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
        Ok(if binding.descriptor.call_name == "a2ui_publish" {
            ToolBindingAdmission::Exclude
        } else {
            ToolBindingAdmission::Allow
        })
    }
}

#[tokio::test]
async fn gateway_respects_tool_policy_during_discovery_and_execution() {
    let model = Model {
        replies: Arc::new(Mutex::new(
            [
                ChatResponse::tool_calls(vec![ToolCall::new(
                    "search",
                    "tool_search",
                    json!({"items":[{"domainCode":"a2ui"}]}),
                )]),
                ChatResponse::tool_calls(vec![ToolCall::new(
                    "publish",
                    "tool_execute",
                    json!({"tool":"a2ui_publish","arguments":{"mode":"Display","messages":[]}}),
                )]),
                ChatResponse::text("done"),
            ]
            .into(),
        )),
        requests: Arc::default(),
    };
    let dir = tempfile::tempdir().unwrap();
    let artifacts = dir.path().join("artifacts");
    std::fs::create_dir(&artifacts).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&artifacts, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let framework = Framework::builder()
        .default_models(false)
        .plugin(LocalStoragePlugin::new(dir.path().join("s.db"), artifacts))
        .plugin(ReactPlugin)
        .plugin(ChatModelPlugin::new(model.clone()))
        .plugin(ToolDiscoveryPlugin::new())
        .plugin(ToolPolicyPlugin::new("test.deny-publish", DenyPublish))
        .capability_pack(A2uiPack::new().unwrap())
        .build()
        .await
        .unwrap();
    let session = framework
        .sessions()
        .unwrap()
        .create(SessionRecord::new("policy"))
        .await
        .unwrap();
    let mut selection = fixture_selection(&framework).unwrap();
    selection.skills = Some(vec![A2UI_SKILL_NAME.into()]);
    let agent = framework.agent(selection).build().await.unwrap();
    let mut request = RunRequest::new(&session.id, "show a report");
    request.extensions.insert(options());
    assert_eq!(
        agent.run(request).await.unwrap().status,
        RunStatus::Completed
    );
    let requests = model.requests.lock().unwrap();
    let discovery = requests[1]
        .messages
        .iter()
        .find(|m| m.role == Role::Tool)
        .unwrap()
        .content
        .text();
    assert!(discovery.contains("a2ui_request_input"));
    assert!(!discovery.contains("a2ui_publish"));
    assert!(
        requests[2]
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Tool)
            .unwrap()
            .tool_result
            .as_ref()
            .unwrap()
            .is_error
    );
    drop(requests);
    assert!(
        framework
            .services()
            .unwrap()
            .get::<A2uiService>()
            .unwrap()
            .state(&session.id)
            .await
            .unwrap()
            .surfaces
            .is_empty()
    );
    assert!(framework.close().await.is_success());
}
#[tokio::test]
async fn form_suspends_resumes_and_publishes_without_duplicate_interaction() {
    let model=Model {replies:Arc::new(Mutex::new([
  ChatResponse::tool_calls(vec![ToolCall::new("form-call","a2ui_request_input",json!({"mode":"SubmitOnce","messages":messages(),"responseSchema":{"type":"object","additionalProperties":false,"required":["name"],"properties":{"name":{"type":"string","minLength":1}}}}))]),
  ChatResponse::tool_calls(vec![ToolCall::new("report-call","a2ui_publish",json!({"mode":"Display","messages":[{"version":VERSION,"createSurface":{"surfaceId":"report","catalogId":AGENT_CATALOG_ID}},{"version":VERSION,"updateComponents":{"surfaceId":"report","components":[{"id":"root","component":"Text","text":"Hello Alice"}]}}]}))]),
  ChatResponse::text("Complete"),
 ].into())),requests:Arc::default()};
    let dir = tempfile::tempdir().unwrap();
    let artifacts = dir.path().join("artifacts");
    std::fs::create_dir(&artifacts).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&artifacts, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let framework = Framework::builder()
        .default_models(false)
        .plugin(LocalStoragePlugin::new(
            dir.path().join("state.db"),
            artifacts,
        ))
        .plugin(ReactPlugin)
        .plugin(ChatModelPlugin::new(model.clone()))
        .capability_pack(A2uiPack::new().unwrap())
        .build()
        .await
        .unwrap();
    let session = framework
        .sessions()
        .unwrap()
        .create(SessionRecord::new("form"))
        .await
        .unwrap();
    let mut selection = fixture_selection(&framework).unwrap();
    selection.skills = Some(vec![A2UI_SKILL_NAME.into()]);
    let agent = framework.agent(selection).build().await.unwrap();
    let mut request = RunRequest::new(&session.id, "Ask my name");
    request.extensions.insert(options());
    let waiting = agent.run(request).await.unwrap();
    assert_eq!(waiting.status, RunStatus::AwaitingInput);
    assert_eq!(waiting.interactions.len(), 1);
    let service = framework.services().unwrap().get::<A2uiService>().unwrap();
    let state = service.state(&session.id).await.unwrap();
    assert_eq!(state.pending_interactions.len(), 1);
    assert!(state.surfaces.is_empty());
    assert!(
        agent
            .answer(
                &session.id,
                waiting.interactions[0].id,
                json!({"unexpected":true})
            )
            .await
            .is_err()
    );
    let original_run = state.pending_interactions[0].run_id.clone();
    let mut resume = agent
        .answer(
            &session.id,
            waiting.interactions[0].id,
            json!({"name":"Alice"}),
        )
        .await
        .unwrap();
    assert_eq!(
        resume.resume.as_ref().unwrap().run_id.to_string(),
        original_run
    );
    resume.extensions.insert(options());
    let done = agent.run(resume).await.unwrap();
    assert_eq!(done.status, RunStatus::Completed);
    let state = service.state(&session.id).await.unwrap();
    assert!(state.pending_interactions.is_empty());
    assert_eq!(state.surfaces.len(), 1);
    assert_eq!(
        state.surfaces[0].snapshot.components["root"]["text"],
        "Hello Alice"
    );
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(
        serde_json::to_string(&requests[1].messages)
            .unwrap()
            .contains("Alice")
    );
    drop(requests);
    framework.close().await;
}

struct Approve;
#[async_trait]
impl ToolPolicy for Approve {
    async fn evaluate(
        &self,
        _: &ToolInvocationContext<'_>,
    ) -> Result<ToolPolicyDecision, AgentError> {
        Ok(ToolPolicyDecision::RequireApproval(
            "Execute operation?".into(),
        ))
    }
}
struct Operation(Arc<std::sync::atomic::AtomicUsize>);
#[async_trait]
impl Tool for Operation {
    async fn call(&self, _: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ToolOutput::text("executed"))
    }
}
#[tokio::test]
async fn native_approval_projection_preserves_execution_authority() {
    for (decision, installed) in [(false, true), (true, true), (false, false), (true, false)] {
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let model = Model {
            replies: Arc::new(Mutex::new(
                [
                    ChatResponse::tool_calls(vec![ToolCall::new(
                        "operation",
                        "operation",
                        json!({}),
                    )]),
                    ChatResponse::text("done"),
                ]
                .into(),
            )),
            requests: Arc::default(),
        };
        let dir = tempfile::tempdir().unwrap();
        let artifacts = dir.path().join("artifacts");
        std::fs::create_dir(&artifacts).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&artifacts, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut builder = Framework::builder()
            .default_models(false)
            .plugin(LocalStoragePlugin::new(dir.path().join("s.db"), artifacts))
            .plugin(ReactPlugin)
            .plugin(ChatModelPlugin::new(model))
            .plugin(ToolProviderPlugin::new(
                "operation",
                StaticToolProvider::new(vec![ToolBinding::new(
                    ToolDescriptor::new(
                        "operation",
                        "operation",
                        "operation",
                        json!({"type":"object"}),
                    ),
                    Operation(count.clone()),
                )]),
            ))
            .plugin(ToolPolicyPlugin::new("approval", Approve));
        if installed {
            builder = builder.capability_pack(A2uiPack::new().unwrap());
        }
        let f = builder.build().await.unwrap();
        let session = f
            .sessions()
            .unwrap()
            .create(SessionRecord::new("approval"))
            .await
            .unwrap();
        let agent = f
            .agent(fixture_selection(&f).unwrap())
            .build()
            .await
            .unwrap();
        let waiting = agent
            .run(RunRequest::new(&session.id, "operate"))
            .await
            .unwrap();
        assert_eq!(waiting.status, RunStatus::AwaitingInput);
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
        let service = f.services().unwrap().get::<A2uiService>();
        if let Some(service) = &service {
            let projected = service
                .state(&session.id)
                .await
                .unwrap()
                .pending_interactions;
            assert_eq!(projected.len(), 1);
            assert_eq!(projected[0].mode, SurfaceMode::SubmitOnce);
            assert_eq!(
                projected[0].messages[1]["updateComponents"]["components"][0]["component"],
                "ApprovalCard"
            );
        }
        let resume = agent
            .answer(&session.id, waiting.interactions[0].id, json!(decision))
            .await
            .unwrap();
        agent.run(resume).await.unwrap();
        assert_eq!(
            count.load(std::sync::atomic::Ordering::SeqCst),
            usize::from(decision)
        );
        assert!(
            agent
                .answer(&session.id, waiting.interactions[0].id, json!(!decision))
                .await
                .is_err()
        );
        if let Some(service) = service {
            assert!(
                service
                    .state(&session.id)
                    .await
                    .unwrap()
                    .pending_interactions
                    .is_empty()
            );
        }
        f.close().await;
    }
}

#[tokio::test]
async fn guidance_skill_tools_and_optional_installation() {
    for (installed, client, selected) in [
        (true, true, true),
        (false, true, true),
        (true, false, true),
        (true, true, false),
    ] {
        let enabled = installed && client && selected;
        let example: Value = serde_json::from_str(include_str!(
            "../skills/a2ui-interface/references/examples.json"
        ))
        .unwrap();
        let mut replies = std::collections::VecDeque::new();
        if enabled {
            for (id, name, args) in [
                ("load", "skill_load", json!({"name":A2UI_SKILL_NAME})),
                (
                    "catalog",
                    "skill_read_resource",
                    json!({"name":A2UI_SKILL_NAME,"path":"catalog.json"}),
                ),
                ("publish", "a2ui_publish", example["display"].clone()),
            ] {
                replies.push_back(ChatResponse::tool_calls(vec![ToolCall::new(
                    id, name, args,
                )]));
            }
        }
        replies.push_back(ChatResponse::text("done"));
        let model = Model {
            replies: Arc::new(Mutex::new(replies)),
            requests: Arc::default(),
        };
        let dir = tempfile::tempdir().unwrap();
        let artifacts = dir.path().join("artifacts");
        std::fs::create_dir(&artifacts).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&artifacts, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut builder = Framework::builder()
            .default_models(false)
            .plugin(LocalStoragePlugin::new(dir.path().join("s.db"), artifacts))
            .plugin(ReactPlugin)
            .plugin(ChatModelPlugin::new(model.clone()));
        if installed {
            builder = builder.capability_pack(A2uiPack::new().unwrap());
        }
        let f = builder.build().await.unwrap();
        let session = f
            .sessions()
            .unwrap()
            .create(SessionRecord::new("optional A2UI"))
            .await
            .unwrap();
        let mut selection = fixture_selection(&f).unwrap();
        selection.skills = Some(if selected {
            vec![A2UI_SKILL_NAME.into()]
        } else {
            vec![]
        });
        let agent = f.agent(selection).build().await.unwrap();
        let mut request = RunRequest::new(&session.id, "Help me with a report");
        if client {
            request.extensions.insert(options());
        }
        let result = agent.run(request).await.unwrap();
        assert_eq!(result.status, RunStatus::Completed);
        let requests = model.requests.lock().unwrap();
        let first = &requests[0];
        for name in ["a2ui_publish", "a2ui_request_input"] {
            assert_eq!(first.tools.iter().any(|t| t.call_name == name), enabled);
        }
        let initial = serde_json::to_string(&first.messages).unwrap();
        assert_eq!(
            initial.contains("a2ui.guidance")
                || initial.contains("A2UI interactive interfaces are available"),
            enabled
        );
        if enabled {
            assert_eq!(requests.len(), 4);
            assert!(initial.contains(A2UI_SKILL_NAME));
            assert!(!initial.contains("Send complete v0.9.1 JSON envelopes"));
            assert!(
                serde_json::to_string(&requests[1].messages)
                    .unwrap()
                    .contains("Send complete v0.9.1 JSON envelopes")
            );
            assert!(
                requests[2]
                    .tools
                    .iter()
                    .any(|t| t.call_name == "a2ui_publish")
            );
            assert!(
                serde_json::to_string(&requests[3].messages)
                    .unwrap()
                    .contains(AGENT_CATALOG_ID)
            );
            assert_eq!(
                f.services()
                    .unwrap()
                    .get::<A2uiService>()
                    .unwrap()
                    .state(&session.id)
                    .await
                    .unwrap()
                    .surfaces
                    .len(),
                1
            );
        } else {
            assert_eq!(requests.len(), 1);
        }
        drop(requests);
        f.close().await;
    }
}
