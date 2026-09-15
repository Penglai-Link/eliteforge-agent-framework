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
impl TokenEstimator for MediaEstimator {
    fn message(&self, _: &ModelRef, message: &Message) -> usize {
        4 + message
            .content
            .0
            .iter()
            .map(|block| match block {
                ContentBlock::Media { .. } => 20,
                ContentBlock::Text { text } => text.len().div_ceil(4),
                _ => 50,
            })
            .sum::<usize>()
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
