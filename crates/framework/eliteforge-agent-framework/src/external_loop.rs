use crate::*;
use async_trait::async_trait;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

/// Delegates execution to an external runtime while retaining Framework's run
/// admission, middleware, lifecycle events, cancellation and timeout path.
pub struct ExternalAgentLoop;

struct ExternalEvents<'a> {
    context: &'a RunContext,
    message_id: String,
    text: Mutex<String>,
    finished: AtomicBool,
}

impl<'a> ExternalEvents<'a> {
    fn new(context: &'a RunContext) -> Self {
        let message_id = RunId::new_v4().to_string();
        context.emit_output(OutputEvent::MessageStarted {
            message_id: message_id.clone(),
        });
        Self {
            context,
            message_id,
            text: Mutex::new(String::new()),
            finished: AtomicBool::new(false),
        }
    }

    fn finish(&self, result: &ExternalAgentResult) {
        let streamed = self.text.lock().unwrap_or_else(|error| error.into_inner());
        let matches = result.content.text().starts_with(streamed.as_str());
        self.context.emit_output(OutputEvent::MessageFinished {
            message_id: self.message_id.clone(),
            content: if matches {
                result.content.clone()
            } else {
                streamed.clone().into()
            },
            structured: if matches {
                result.structured.clone()
            } else {
                None
            },
            tool_calls: Vec::new(),
        });
        self.finished.store(true, Ordering::Release);
    }
}

impl Drop for ExternalEvents<'_> {
    fn drop(&mut self) {
        if !self.finished.load(Ordering::Acquire) {
            self.context.emit_output(OutputEvent::MessageAborted {
                message_id: self.message_id.clone(),
            });
        }
    }
}

#[async_trait]
impl ExternalAgentEventSink for ExternalEvents<'_> {
    async fn emit(&self, event: ExternalAgentEvent) {
        match &event {
            ExternalAgentEvent::TextDelta(delta) => {
                {
                    let mut text = self.text.lock().unwrap_or_else(|error| error.into_inner());
                    text.push_str(delta);
                    self.context.emit_output(OutputEvent::TextDelta {
                        message_id: self.message_id.clone(),
                        delta: delta.clone(),
                    });
                }
                self.context.text_delta(delta).await;
            }
            ExternalAgentEvent::ReasoningDelta(delta) => {
                self.context.emit_output(OutputEvent::ReasoningDelta {
                    message_id: self.message_id.clone(),
                    delta: delta.clone(),
                });
                self.context.reasoning_delta(delta).await;
            }
            _ => self.context.emit_output(OutputEvent::Custom {
                name: "framework.external_agent".into(),
                value: serde_json::to_value(&event).expect("external observation is serializable"),
            }),
        }
        self.context.emit(AgentEvent::ExternalAgent(event)).await;
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
        let events = ExternalEvents::new(context);
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
                &events,
            )
            .await?;
        let output = result.content.text();
        if let Some(session) = &result.session {
            events
                .emit(ExternalAgentEvent::Session(session.clone()))
                .await;
        }
        events.finish(&result);
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
