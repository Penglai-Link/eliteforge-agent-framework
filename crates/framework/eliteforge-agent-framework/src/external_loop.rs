use crate::*;
use async_trait::async_trait;

/// Delegates execution to an external runtime while retaining Framework's run
/// admission, middleware, lifecycle events, cancellation and timeout path.
pub struct ExternalAgentLoop;

struct ExternalEvents<'a>(&'a RunContext);

#[async_trait]
impl ExternalAgentEventSink for ExternalEvents<'_> {
    async fn emit(&self, event: ExternalAgentEvent) {
        match &event {
            ExternalAgentEvent::TextDelta(delta) => self.0.text_delta(delta).await,
            ExternalAgentEvent::ReasoningDelta(delta) => self.0.reasoning_delta(delta).await,
            _ => {}
        }
        self.0.emit(AgentEvent::ExternalAgent(event)).await;
    }
}

#[async_trait]
impl AgentLoop for ExternalAgentLoop {
    fn supports_steering(&self) -> bool {
        false
    }
    fn requires_conversations(&self) -> bool {
        false
    }
    fn validate_services(&self, services: &AgentServices) -> Result<(), AgentError> {
        services
            .external_agent
            .as_ref()
            .map(|_| ())
            .ok_or(AgentError::MissingService("ExternalAgent"))
    }
    async fn run(
        &self,
        request: &RunRequest,
        context: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let external = services
            .external_agent
            .as_ref()
            .ok_or(AgentError::MissingService("ExternalAgent"))?;
        let session = request
            .extensions
            .get::<ExternalSessionRef>()
            .map(|value| (*value).clone());
        let result = external
            .run(
                ExternalAgentRequest {
                    input: request.input.clone(),
                    model: request
                        .model
                        .as_ref()
                        .map(|model| format!("{}/{}", model.connection, model.model)),
                    session,
                    extensions: request.extensions.clone(),
                },
                &ExternalEvents(context),
            )
            .await?;
        let output = result.content.text();
        if let Some(session) = &result.session {
            context
                .emit(AgentEvent::ExternalAgent(ExternalAgentEvent::Session(
                    session.clone(),
                )))
                .await;
        }
        Ok(RunResult {
            output,
            content: result.content,
            structured: result.structured,
            interactions: Vec::new(),
            status: RunStatus::Completed,
            usage: result.usage,
            steps: 1,
            tool_calls: 0,
            external_session: result.session,
        })
    }
}
