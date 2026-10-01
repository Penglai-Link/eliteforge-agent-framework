//! Bridges transport observations into the Framework ExternalAgent extension.
//! Neither adapter executes Framework tools or stores external session state.
use std::sync::Arc;

use async_trait::async_trait;
use eliteforge_agent_core as fw;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::{
    ExternalAgentEvent as TransportEvent, ExternalAgentEventKind as TransportEventKind,
    ExternalAgentUsage, ExternalCli, ExternalCliRequest, JsonAgentBridge, TransportError,
};

pub struct CliExternalAgent {
    cli: ExternalCli,
    /// Native CLI model code. Framework's model reference names the execution
    /// engine and must not be passed to a CLI as though it were a vendor model.
    native_model: Option<String>,
}

impl CliExternalAgent {
    pub fn new(cli: ExternalCli, native_model: Option<String>) -> Self {
        Self { cli, native_model }
    }
}

#[async_trait]
impl fw::ExternalAgent for CliExternalAgent {
    async fn run(
        &self,
        request: fw::ExternalAgentRequest,
        sink: &dyn fw::ExternalAgentEventSink,
    ) -> Result<fw::ExternalAgentResult, fw::AgentError> {
        let (sender, mut receiver) = mpsc::channel(1024);
        let overflow = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let callback_overflow = overflow.clone();
        let observed = Arc::new(move |event| {
            if sender.try_send(event).is_err() {
                callback_overflow.store(true, std::sync::atomic::Ordering::Release);
            }
        });
        let context = ExternalCliRequest {
            prompt: request.input.protocol_text(),
            model: self.native_model.clone(),
            session_id: request
                .session
                .as_ref()
                .map(|session| session.session_id.clone()),
        };
        let invocation = self.cli.invoke(context, Some(observed));
        tokio::pin!(invocation);
        let result = loop {
            if overflow.load(std::sync::atomic::Ordering::Acquire) {
                return Err(fw::AgentError::Run(
                    "external event queue capacity exceeded".into(),
                ));
            }
            tokio::select! {
                event = receiver.recv() => {
                    if let Some(event) = event {
                        forward_event(event, sink).await;
                    } else {
                        break invocation.await;
                    }
                }
                result = &mut invocation => break result,
            }
        };
        if overflow.load(std::sync::atomic::Ordering::Acquire) {
            return Err(fw::AgentError::Run(
                "external event queue capacity exceeded".into(),
            ));
        }
        while let Ok(event) = receiver.try_recv() {
            forward_event(event, sink).await;
        }
        let result = result.map_err(transport_error)?;
        Ok(fw::ExternalAgentResult {
            content: result.output.into(),
            structured: None,
            session: result.session_id.map(|session_id| fw::ExternalSessionRef {
                session_id,
                run_id: result.run_id,
            }),
            usage: usage(result.usage),
        })
    }
}

async fn forward_event(event: TransportEvent, sink: &dyn fw::ExternalAgentEventSink) {
    let event = match event.kind {
        TransportEventKind::Text => fw::ExternalAgentEvent::TextDelta(event.delta),
        TransportEventKind::ReasoningSummary => fw::ExternalAgentEvent::ReasoningDelta(event.delta),
        TransportEventKind::Status => fw::ExternalAgentEvent::Status(event.delta),
        TransportEventKind::SessionObserved => {
            fw::ExternalAgentEvent::Session(fw::ExternalSessionRef {
                session_id: event.delta,
                run_id: None,
            })
        }
        TransportEventKind::ToolObserved => fw::ExternalAgentEvent::Tool {
            name: event.delta,
            detail: event.detail.unwrap_or(Value::Null),
        },
        TransportEventKind::ApprovalObserved => fw::ExternalAgentEvent::Approval {
            detail: event.detail.unwrap_or(Value::Null),
        },
    };
    sink.emit(event).await;
}

fn usage(value: Option<ExternalAgentUsage>) -> fw::Usage {
    value.map_or_else(fw::Usage::default, |value| fw::Usage {
        input_tokens: u64::from(value.input_tokens),
        output_tokens: u64::from(value.output_tokens),
    })
}

fn transport_error(error: TransportError) -> fw::AgentError {
    fw::AgentError::Run(format!("external agent transport failed: {error}"))
}

/// Product-specific bridges project their authorized request and response
/// envelopes here. The bridge transport only provides start/resume/cancel URLs.
pub trait JsonBridgeProjection: Send + Sync {
    fn request(&self, request: &fw::ExternalAgentRequest) -> Result<Value, fw::AgentError>;
    fn cancel_body(&self, _request: &fw::ExternalAgentRequest) -> Value {
        Value::Object(Default::default())
    }
    fn observations(&self, _response: &Value) -> Vec<fw::ExternalAgentEvent> {
        Vec::new()
    }
    fn result(&self, response: Value) -> Result<fw::ExternalAgentResult, fw::AgentError>;
}

pub struct JsonBridgeExternalAgent {
    bridge: JsonAgentBridge,
    projection: Arc<dyn JsonBridgeProjection>,
}

impl JsonBridgeExternalAgent {
    pub fn new(bridge: JsonAgentBridge, projection: Arc<dyn JsonBridgeProjection>) -> Self {
        Self { bridge, projection }
    }
}

#[async_trait]
impl fw::ExternalAgent for JsonBridgeExternalAgent {
    async fn run(
        &self,
        request: fw::ExternalAgentRequest,
        sink: &dyn fw::ExternalAgentEventSink,
    ) -> Result<fw::ExternalAgentResult, fw::AgentError> {
        let body = self.projection.request(&request)?;
        let cancel_body = self.projection.cancel_body(&request);
        let response = self
            .bridge
            .invoke_with_known_run(
                request.session.is_some(),
                &body,
                request
                    .session
                    .as_ref()
                    .and_then(|session| session.run_id.as_deref()),
                &cancel_body,
            )
            .await
            .map_err(transport_error)?;
        for event in self.projection.observations(&response) {
            sink.emit(event).await;
        }
        self.projection.result(response)
    }
}
