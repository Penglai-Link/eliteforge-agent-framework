//! Framework-owned native session restoration for an external Agent model stage.
use async_trait::async_trait;
use eliteforge_agent_core as fw;
use std::sync::Arc;

pub const CONNECTION: &str = "framework.external";
pub const CHECKPOINT_FORMAT: &str = "framework.external-session.v1";
/// Product adapters project authorized context into a CLI/bridge request and
/// return the native result. They do not decide checkpoint restoration/retry.
#[async_trait]
pub trait ExternalAgentInvocation: Send + Sync {
    async fn invoke(
        &self,
        session: Option<String>,
        stream: &dyn fw::ChatStream,
    ) -> Result<ExternalStageResult, fw::ModelError>;
}
pub struct ExternalStageResult {
    pub output: String,
    pub usage: fw::Usage,
    pub session_id: Option<String>,
    pub payload: Option<serde_json::Value>,
}
#[derive(Clone)]
pub struct ExternalStage {
    /// Opaque host-computed identity of the authorized backend/principal/model scope.
    pub scope: String,
    pub session_id: Option<String>,
    pub require_resume: bool,
    pub invocation: Arc<dyn ExternalAgentInvocation>,
}
pub struct ExternalAgentModel;
#[async_trait]
impl fw::ChatModel for ExternalAgentModel {
    async fn generate(
        &self,
        model_id: &str,
        request: fw::ChatRequest,
        stream: &dyn fw::ChatStream,
    ) -> Result<fw::ChatResponse, fw::ModelError> {
        let stage = request
            .extensions
            .get::<ExternalStage>()
            .ok_or_else(|| fw::ModelError::Fatal("external invocation is missing".into()))?;
        let model = fw::ModelRef::new(CONNECTION, model_id);
        let session = stage.session_id.clone().or_else(|| {
            request
                .messages
                .iter()
                .rev()
                .filter_map(|message| message.model_context.as_ref())
                .find(|state| {
                    state.format == CHECKPOINT_FORMAT
                        && state.model == model
                        && state.data["scope"].as_str() == Some(stage.scope.as_str())
                })
                .and_then(|state| state.data["sessionId"].as_str())
                .map(str::to_owned)
        });
        if stage.require_resume && session.is_none() {
            return Err(fw::ModelError::Fatal(
                "external resume session is missing".into(),
            ));
        }
        let result = stage.invocation.invoke(session, stream).await?;
        Ok(fw::ChatResponse {
            content: result.output.into(),
            structured: result.payload,
            usage: result.usage,
            model_context: Some(fw::ModelMessageContext {
                model,
                protocol: "external-agent".into(),
                format: CHECKPOINT_FORMAT.into(),
                data: serde_json::json!({"scope": stage.scope, "sessionId": result.session_id}),
            }),
            ..Default::default()
        })
    }
}
/// Installed through the public external-agent capability pack.
pub fn model_plugin() -> fw::ModelConnectionPlugin {
    fw::ModelConnectionPlugin::new(
        fw::ModelConnection::new(
            CONNECTION,
            Arc::new(ExternalAgentModel),
            vec![
                fw::ModelDescriptor::new("agent")
                    .with_input_modalities([fw::Modality::Text, fw::Modality::Image]),
            ],
        )
        .with_protocol("external-agent"),
    )
}
