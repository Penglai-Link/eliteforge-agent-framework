use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_test_support::*;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Notify;

struct MediaEstimator;
#[async_trait]
impl TokenEstimator for MediaEstimator {
    async fn message(&self, _: &ModelRef, message: &Message) -> Result<usize, ModelError> {
        Ok(4 + message
            .content
            .0
            .iter()
            .map(|block| match block {
                ContentBlock::Media { .. } => 20,
                ContentBlock::Text { text } => text.len().div_ceil(4),
                _ => 50,
            })
            .sum::<usize>())
    }
}
struct ModalityRouter;
#[async_trait]
impl ModelSelectionStrategy for ModalityRouter {
    async fn select(
        &self,
        context: &ModelSelectionContext<'_>,
        _: &ModelCatalog,
    ) -> Result<ModelRef, AgentError> {
        Ok(ModelRef::new(
            "shared",
            if context.messages.iter().any(|m| m.content.has_media()) {
                "vision"
            } else {
                "text"
            },
        ))
    }
}
struct Recorder(Arc<Mutex<Vec<(String, ChatRequest)>>>);
#[async_trait]
impl ChatModel for Recorder {
    async fn generate(
        &self,
        model: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        self.0.lock().unwrap().push((model.into(), request));
        Ok(ChatResponse::text("result"))
    }
}
#[tokio::test]
async fn model_aware_estimator_and_modality_routing_reach_final_admission() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let connection = ModelConnection::new(
        "shared",
        Arc::new(Recorder(calls.clone())),
        vec![
            ModelDescriptor::new("text"),
            ModelDescriptor::new("vision").with_input_modalities([Modality::Text, Modality::Image]),
        ],
    )
    .with_token_estimator(Arc::new(MediaEstimator));
    let agent = TestBuilder::default()
        .plugin(ModelConnectionPlugin::new(connection))
        .plugin(ModelStrategyPlugin(Arc::new(ModalityRouter)))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new(
        "media",
        Content(vec![ContentBlock::Media {
            media: MediaContent {
                mime_type: "image/png".into(),
                source: MediaSource::Url("https://fixture.invalid/image.png".into()),
                name: None,
                estimated_tokens: None,
            },
        }]),
    );
    request.limits.max_context_tokens = 30;
    request.limits.max_total_tokens = 40;
    agent.run(request).await.unwrap();
    {
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "vision");
        assert_eq!(calls[0].1.options.max_output_tokens, Some(16));
    }
    agent.close().await;
}
#[tokio::test]
async fn unsupported_structured_output_fails_before_provider_dispatch() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut descriptor = ModelDescriptor::new("text");
    descriptor.supports_structured_output = Some(false);
    let agent = TestBuilder::default()
        .plugin(ModelConnectionPlugin::new(ModelConnection::new(
            "shared",
            Arc::new(Recorder(calls.clone())),
            vec![descriptor],
        )))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("s", "json");
    request.generation.output_schema = Some(OutputSchema {
        name: "result".into(),
        schema: json!({"type":"object"}),
    });
    assert!(
        agent
            .run(request)
            .await
            .unwrap_err()
            .to_string()
            .contains("no compatible chat model")
    );
    assert!(calls.lock().unwrap().is_empty());
    agent.close().await;
}

struct RevokeModel;
#[async_trait]
impl ChatModel for RevokeModel {
    async fn generate(
        &self,
        _model_id: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        if request.messages.iter().any(|m| m.role == Role::Tool) {
            assert!(!request.tools.iter().any(|t| t.id == "victim"));
            let last = request.messages.last().unwrap();
            assert!(last.tool_result.as_ref().unwrap().is_error);
            Ok(ChatResponse::text("revocation enforced"))
        } else {
            Ok(ChatResponse::tool_calls(vec![
                ToolCall::new("one", "revoke", json!({})),
                ToolCall::new("two", "victim", json!({})),
            ]))
        }
    }
}
struct Revoker;
#[async_trait]
impl Tool for Revoker {
    async fn call(&self, _: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            effect: ToolEffect::RevokeTools(vec!["victim".into()]),
            ..ToolOutput::text("revoked")
        })
    }
}
struct Victim(Arc<AtomicUsize>);
#[async_trait]
impl Tool for Victim {
    async fn call(&self, _: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::text("effect"))
    }
}
#[tokio::test]
async fn tool_revocation_blocks_pending_sibling_and_next_model_exposure() {
    let calls = Arc::new(AtomicUsize::new(0));
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(RevokeModel))
        .plugin(ToolProviderPlugin::new(
            "tools",
            StaticToolProvider::new(vec![
                ToolBinding::new(
                    ToolDescriptor::new("revoke", "revoke", "revoke", json!({"type":"object"})),
                    Revoker,
                ),
                ToolBinding::new(
                    ToolDescriptor::new("victim", "victim", "effect", json!({"type":"object"})),
                    Victim(calls.clone()),
                ),
            ]),
        ))
        .build()
        .await
        .unwrap();
    assert_eq!(
        agent.run(RunRequest::new("s", "x")).await.unwrap().output,
        "revocation enforced"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    agent.close().await;
}

struct OneToolModel;
#[async_trait]
impl ChatModel for OneToolModel {
    async fn generate(
        &self,
        _model_id: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        if request.messages.iter().any(|m| m.role == Role::Tool) {
            Ok(ChatResponse::text("done"))
        } else {
            Ok(ChatResponse::tool_calls(vec![ToolCall::new(
                "one",
                "victim",
                json!({}),
            )]))
        }
    }
}
struct Gate {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}
#[async_trait]
impl ToolMiddleware for Gate {
    async fn intercept(
        &self,
        _: &ToolInvocationContext<'_>,
        next: ToolNext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        self.entered.notify_one();
        self.release.notified().await;
        next.run().await
    }
}
#[tokio::test]
async fn host_revocation_during_middleware_is_rechecked_before_external_effect() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(OneToolModel))
        .plugin(ToolMiddlewarePlugin::new(
            "gate",
            Gate {
                entered: entered.clone(),
                release: release.clone(),
            },
        ))
        .plugin(ToolProviderPlugin::new(
            "tools",
            StaticToolProvider::new(vec![ToolBinding::new(
                ToolDescriptor::new("victim", "victim", "effect", json!({"type":"object"})),
                Victim(calls.clone()),
            )]),
        ))
        .build()
        .await
        .unwrap();
    let handle = agent.start(RunRequest::new("s", "x")).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    let control = agent.control(&handle).unwrap();
    let waiting = tokio::spawn({
        let agent = agent.clone();
        async move { agent.wait(handle).await }
    });
    agent.revoke_tools(&control, &["victim".into()]).unwrap();
    release.notify_one();
    waiting.await.unwrap().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    agent.close().await;
}

struct TemporaryExposureModel(AtomicUsize);
#[async_trait]
impl ChatModel for TemporaryExposureModel {
    async fn generate(
        &self,
        _: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        let step = self.0.fetch_add(1, Ordering::SeqCst);
        let child_visible = request.tools.iter().any(|tool| tool.id == "child");
        assert_eq!(child_visible, step == 1 || step == 3);
        let call = match step {
            0 | 2 => ToolCall::new(format!("expand-{step}"), "expand", json!({})),
            1 => ToolCall::new("child-provider-id", "child", json!({})),
            _ => return Ok(ChatResponse::text("temporary exposure restored")),
        };
        Ok(ChatResponse::tool_calls(vec![call]))
    }
}
struct ExposureTool {
    child: bool,
    seen: Arc<Mutex<Vec<ToolCallIdentity>>>,
}
#[async_trait]
impl Tool for ExposureTool {
    async fn call(&self, _: Value, run: &RunContext) -> Result<ToolOutput, ToolError> {
        let identity = run
            .extensions
            .get::<ToolCallIdentity>()
            .expect("executor invocation identity");
        assert_eq!(
            identity.tool_id,
            if self.child { "child" } else { "expand" }
        );
        assert!(!identity.invocation_id.is_empty());
        if self.child {
            assert_eq!(identity.provider_call_id, "child-provider-id");
        }
        self.seen.lock().unwrap().push((*identity).clone());
        Ok(ToolOutput {
            content: "ok".into(),
            effect: ToolEffect::SetExposedTools(if self.child {
                vec!["expand".into()]
            } else {
                vec!["expand".into(), "child".into()]
            }),
            ..ToolOutput::default()
        })
    }
}
#[tokio::test]
async fn temporary_exposure_can_collapse_and_reexpand_without_revoking_authority() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(TemporaryExposureModel(
            AtomicUsize::new(0),
        )))
        .plugin(ToolProviderPlugin::new(
            "scoped-tools",
            StaticToolProvider::new(vec![
                ToolBinding::new(
                    ToolDescriptor::new("expand", "expand", "discover", json!({"type":"object"})),
                    ExposureTool {
                        child: false,
                        seen: seen.clone(),
                    },
                ),
                ToolBinding::new(
                    ToolDescriptor::new("child", "child", "host tool", json!({"type":"object"})),
                    ExposureTool {
                        child: true,
                        seen: seen.clone(),
                    },
                )
                .hidden(),
            ]),
        ))
        .build()
        .await
        .unwrap();
    let request = RunRequest::new("exposure-session", "expand, use, collapse, expand");
    let extensions = request.extensions.clone();
    assert_eq!(
        agent.run(request).await.unwrap().output,
        "temporary exposure restored"
    );
    let identities = seen.lock().unwrap();
    assert_eq!(identities.len(), 3);
    assert_eq!(
        identities
            .iter()
            .map(|identity| &identity.invocation_id)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        3
    );
    assert!(
        extensions.get::<ToolCallIdentity>().is_none(),
        "call identity must not leak into the parent run"
    );
    drop(identities);
    agent.close().await;
}

struct BatchModel;
#[async_trait]
impl ChatModel for BatchModel {
    async fn generate(
        &self,
        _: &str,
        _: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        Ok(ChatResponse::tool_calls(
            (0..3)
                .map(|i| ToolCall::new(format!("call-{i}"), "victim", json!({})))
                .collect(),
        ))
    }
}
struct ProjectResults(Arc<Mutex<Vec<(String, bool)>>>);
#[async_trait]
impl ToolResultProcessor for ProjectResults {
    async fn process(
        &self,
        call: &ToolInvocationContext<'_>,
        output: ToolOutput,
    ) -> Result<ToolOutput, AgentError> {
        self.0
            .lock()
            .unwrap()
            .push((call.call.id.clone(), output.is_error));
        Ok(output)
    }
}
#[tokio::test]
async fn partial_batch_admission_records_unexecuted_siblings_and_stops_at_budget() {
    for policy in [
        ToolBatchBudgetPolicy::RejectBatch,
        ToolBatchBudgetPolicy::ExecuteAvailable,
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let results = Arc::new(Mutex::new(Vec::new()));
        let agent = TestBuilder::default()
            .plugin(ChatModelPlugin::new(BatchModel))
            .plugin(ToolProviderPlugin::new(
                "tools",
                StaticToolProvider::new(vec![ToolBinding::new(
                    ToolDescriptor::new("victim", "victim", "effect", json!({"type":"object"})),
                    Victim(calls.clone()),
                )]),
            ))
            .build()
            .await
            .unwrap();
        let mut request = RunRequest::new("batch-budget", "go");
        request.limits.max_tool_calls = 2;
        request.limits.tool_batch_budget_policy = policy;
        request
            .extensions
            .insert(ToolResultProcessorService(Arc::new(ProjectResults(
                results.clone(),
            ))));
        let result = agent.run(request).await.unwrap();
        assert_eq!(result.status, RunStatus::ToolCallLimit);
        let expected = if policy == ToolBatchBudgetPolicy::RejectBatch {
            0
        } else {
            2
        };
        assert_eq!(calls.load(Ordering::SeqCst), expected);
        assert_eq!(result.tool_calls, expected);
        if expected > 0 {
            assert_eq!(
                *results.lock().unwrap(),
                vec![
                    ("call-0".into(), false),
                    ("call-1".into(), false),
                    ("call-2".into(), true)
                ]
            );
            let history = agent
                .conversation(&ConversationKey::new("batch-budget"))
                .await
                .unwrap();
            let rejected = history
                .messages
                .iter()
                .find(|m| m.tool_call_id.as_deref() == Some("call-2"))
                .unwrap();
            assert!(rejected.tool_result.as_ref().unwrap().is_error);
            assert!(rejected.content.text().contains("not executed"));
        }
        agent.close().await;
    }
}
struct HostInput {
    pending: Mutex<Vec<Content>>,
    sealed: std::sync::atomic::AtomicBool,
}
#[async_trait]
impl LiveInputSource for HostInput {
    async fn drain(&self, _: &RunContext, seal: bool) -> Result<Vec<Content>, AgentError> {
        let pending = std::mem::take(&mut *self.pending.lock().unwrap());
        if seal && pending.is_empty() {
            self.sealed.store(true, Ordering::SeqCst);
        }
        Ok(pending)
    }
}
struct SteeringModel {
    host: Arc<HostInput>,
    calls: AtomicUsize,
}
#[async_trait]
impl ChatModel for SteeringModel {
    async fn generate(
        &self,
        _: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.host.pending.lock().unwrap().push(Content(vec![
                ContentBlock::Text {
                    text: "inspect attached image".into(),
                },
                ContentBlock::Media {
                    media: MediaContent {
                        mime_type: "image/png".into(),
                        source: MediaSource::Base64("aGVsbG8=".into()),
                        name: Some("attachment.png".into()),
                        estimated_tokens: Some(5),
                    },
                },
            ]));
            Ok(ChatResponse::text("first answer"))
        } else {
            let last = request.messages.last().unwrap();
            assert_eq!(last.role, Role::User);
            assert!(last.content.has_media());
            assert!(last.content.text().contains("inspect attached image"));
            assert!(
                request
                    .messages
                    .iter()
                    .any(|m| m.content.text() == "first answer")
            );
            Ok(ChatResponse::text("attachment handled"))
        }
    }
}
#[tokio::test]
async fn host_steering_preserves_typed_content_and_atomically_seals_final_admission() {
    let host = Arc::new(HostInput {
        pending: Mutex::new(vec![]),
        sealed: std::sync::atomic::AtomicBool::new(false),
    });
    let agent = TestBuilder::default()
        .plugin(ModelConnectionPlugin::new(ModelConnection::new(
            "host",
            Arc::new(SteeringModel {
                host: host.clone(),
                calls: AtomicUsize::new(0),
            }),
            vec![
                ModelDescriptor::new("vision")
                    .with_input_modalities([Modality::Text, Modality::Image]),
            ],
        )))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("typed-steering", "first question");
    request
        .extensions
        .insert(LiveInputSourceService(host.clone()));
    let result = agent.run(request).await.unwrap();
    assert_eq!(result.steps, 2);
    assert_eq!(result.output, "attachment handled");
    assert!(host.sealed.load(Ordering::SeqCst));
    let history = agent
        .conversation(&ConversationKey::new("typed-steering"))
        .await
        .unwrap();
    assert_eq!(
        history
            .messages
            .iter()
            .filter(|m| m.content.has_media())
            .count(),
        1
    );
    agent.close().await;
}

#[tokio::test]
async fn hidden_tool_authority_requires_explicit_host_opt_in() {
    for allowed in [false, true] {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut binding = ToolBinding::new(
            ToolDescriptor::new(
                "victim",
                "victim",
                "hidden authorized tool",
                json!({"type":"object"}),
            ),
            Victim(calls.clone()),
        );
        binding.exposed = false;
        let agent = TestBuilder::default()
            .plugin(ChatModelPlugin::new(OneToolModel))
            .plugin(ToolProviderPlugin::new(
                "hidden",
                StaticToolProvider::new(vec![binding]),
            ))
            .build()
            .await
            .unwrap();
        let mut request = RunRequest::new("hidden", "call an authorized hidden tool");
        request.limits.allow_unexposed_tools = allowed;
        agent.run(request).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), usize::from(allowed));
        agent.close().await;
    }
}
