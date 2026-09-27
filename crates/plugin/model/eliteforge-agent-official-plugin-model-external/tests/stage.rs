use eliteforge_agent_core::{self as fw, ChatModel};
use eliteforge_agent_official_plugin_model_external::*;
use std::sync::{Arc, Mutex};
struct Stream;
#[async_trait::async_trait]
impl fw::ChatStream for Stream {
    async fn text_delta(&self, _: &str) {}
    async fn reasoning_delta(&self, _: &str) {}
}
#[derive(Default)]
struct Invocation(Mutex<Vec<Option<String>>>);
#[async_trait::async_trait]
impl ExternalAgentInvocation for Invocation {
    async fn invoke(
        &self,
        session: Option<String>,
        _: &dyn fw::ChatStream,
    ) -> Result<ExternalStageResult, fw::ModelError> {
        self.0.lock().unwrap().push(session);
        Ok(ExternalStageResult {
            output: "answer".into(),
            usage: fw::Usage::default(),
            session_id: Some("native-42".into()),
            payload: Some(serde_json::json!({"business": "preserved"})),
        })
    }
}
async fn call(
    invocation: Arc<Invocation>,
    messages: Vec<fw::Message>,
    scope: &str,
    session: Option<&str>,
    require_resume: bool,
) -> Result<fw::ChatResponse, fw::ModelError> {
    let mut extensions = fw::Extensions::default();
    extensions.insert(ExternalStage {
        scope: scope.into(),
        session_id: session.map(str::to_owned),
        require_resume,
        invocation,
    });
    ExternalAgentModel
        .generate(
            "agent",
            fw::ChatRequest {
                options: Default::default(),
                messages,
                tools: vec![],
                extensions,
            },
            &Stream,
        )
        .await
}
#[tokio::test]
async fn session_restoration_requires_matching_model_and_scope() {
    let invocation = Arc::new(Invocation::default());
    let first = call(invocation.clone(), vec![], "owner-a", None, false)
        .await
        .unwrap();
    assert_eq!(first.structured.unwrap()["business"], "preserved");
    let mut message = fw::Message::assistant("answer");
    message.model_context = first.model_context;
    call(
        invocation.clone(),
        vec![message.clone()],
        "owner-a",
        None,
        false,
    )
    .await
    .unwrap();
    call(
        invocation.clone(),
        vec![message.clone()],
        "owner-b",
        None,
        false,
    )
    .await
    .unwrap();
    call(
        invocation.clone(),
        vec![message.clone()],
        "owner-a",
        Some("explicit"),
        true,
    )
    .await
    .unwrap();
    message.model_context.as_mut().unwrap().model = fw::ModelRef::new("different", "agent");
    call(invocation.clone(), vec![message], "owner-a", None, false)
        .await
        .unwrap();
    assert_eq!(
        *invocation.0.lock().unwrap(),
        vec![
            None,
            Some("native-42".into()),
            None,
            Some("explicit".into()),
            None
        ]
    );
}
#[tokio::test]
async fn missing_resume_session_never_invokes_transport() {
    let invocation = Arc::new(Invocation::default());
    assert!(
        call(invocation.clone(), vec![], "owner", None, true)
            .await
            .is_err()
    );
    assert!(invocation.0.lock().unwrap().is_empty());
}
