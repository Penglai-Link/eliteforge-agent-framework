use crate::{accumulator::StreamAccumulator, protocol::adapter_for_protocol, *};
use eliteforge_agent_core as fw;
use std::sync::{Arc, Mutex};

/// Protocol execution only; there is no retry or implicit non-stream fallback.
pub struct HttpProtocolClient {
    transport: Arc<dyn ModelTransport>,
}
impl HttpProtocolClient {
    pub fn new(transport: Arc<dyn ModelTransport>) -> Self {
        Self { transport }
    }
    pub async fn generate(
        &self,
        config: &AgentModelConfig,
        request: &ModelRequest,
    ) -> Result<ModelResponse, fw::ModelError> {
        let adapter = adapter_for_protocol(config.protocol)
            .ok_or_else(|| fw::ModelError::Fatal("unsupported model protocol".into()))?;
        let body = adapter
            .request_body(config, request)
            .map_err(fw::ModelError::Fatal)?;
        let result = self
            .transport
            .json(
                config.api_path.as_deref().unwrap_or(adapter.default_path()),
                &body,
            )
            .await?;
        adapter
            .parse_response(&result)
            .map_err(fw::ModelError::Fatal)
    }
    pub async fn generate_stream(
        &self,
        config: &AgentModelConfig,
        request: &ModelRequest,
        sink: Arc<dyn Fn(ModelStreamEvent) + Send + Sync>,
    ) -> Result<ModelResponse, fw::ModelError> {
        let adapter = adapter_for_protocol(config.protocol)
            .ok_or_else(|| fw::ModelError::Fatal("unsupported model protocol".into()))?;
        let body = adapter
            .stream_request_body(config, request)
            .map_err(fw::ModelError::Fatal)?;
        let state = Arc::new(Mutex::new((StreamAccumulator::default(), false, 0usize)));
        let callback_state = state.clone();
        let protocol = config.protocol;
        self.transport
            .events(
                config.api_path.as_deref().unwrap_or(adapter.default_path()),
                &body,
                Arc::new(move |event| {
                    let mut state = callback_state.lock().unwrap_or_else(|e| e.into_inner());
                    let first_event = state.2 == 0;
                    state.2 = state.2.saturating_add(
                        serde_json::to_vec(&event)
                            .map_err(|_| fw::ModelError::Fatal("invalid model event".into()))?
                            .len(),
                    );
                    if state.2 > 16 * 1024 * 1024 {
                        return Err(fw::ModelError::Fatal(
                            "model stream exceeds total byte limit".into(),
                        ));
                    }
                    if let Some(error) = stream_failure(&event, first_event) {
                        return Err(error);
                    }
                    if let Some(usage) = adapter.parse_usage(&event) {
                        state.0.token_usage = Some(usage);
                    }
                    if let Some(response) = adapter
                        .parse_stream_terminal_response(&event)
                        .map_err(fw::ModelError::Fatal)?
                    {
                        state.0.terminal_response = Some(response);
                        state.1 = true;
                    }
                    if let Some(reason) = event
                        .pointer("/choices/0/finish_reason")
                        .and_then(serde_json::Value::as_str)
                    {
                        if !matches!(reason, "stop" | "tool_calls" | "function_call") {
                            return Err(fw::ModelError::Fatal(
                                "model output stopped abnormally".into(),
                            ));
                        }
                        state.1 = true;
                    }
                    state.0.observe_raw_event(protocol, &event);
                    for delta in adapter
                        .parse_stream_events(&event)
                        .map_err(fw::ModelError::Fatal)?
                    {
                        state.0.observe_delta(&delta);
                        sink(delta);
                    }
                    Ok(())
                }),
            )
            .await?;
        let (state, terminal, _) = state.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if !terminal {
            return Err(fw::ModelError::Indeterminate(
                "model stream ended without terminal response".into(),
            ));
        }
        let usage = state.token_usage;
        let mut response = if let Some(response) = state.terminal_response.clone() {
            response
        } else {
            state.into_response().ok_or_else(|| {
                fw::ModelError::Indeterminate("model stream completed without output".into())
            })?
        };
        if usage.is_some() {
            response.token_usage = usage;
        }
        Ok(response)
    }
}

fn stream_failure(event: &serde_json::Value, first_event: bool) -> Option<fw::ModelError> {
    let has_error = event.get("error").is_some_and(|error| !error.is_null());
    let is_failure = event
        .get("type")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|kind| matches!(kind, "error" | "response.failed" | "response.incomplete"));
    if !has_error && !is_failure {
        return None;
    }

    // Keep provider diagnostics verbatim, including unknown error fields, but
    // do not copy the full generated output or request settings into the error.
    let mut details = event.clone();
    if let Some(response) = details
        .get_mut("response")
        .and_then(serde_json::Value::as_object_mut)
    {
        response.retain(|key, _| {
            matches!(
                key.as_str(),
                "id" | "status" | "error" | "incomplete_details"
            )
        });
    }
    // A first-frame model rejection is a definite pre-generation failure.
    // Keep every other stream failure indeterminate, including partial output.
    let error = event
        .get("error")
        .or_else(|| event.pointer("/response/error"));
    let code = error
        .and_then(|value| value.get("code"))
        .and_then(serde_json::Value::as_str);
    let message = error
        .and_then(|value| value.get("message"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let model_rejected = matches!(code, Some("model_not_found" | "model_not_available"))
        || message.contains("The model does not exist or you do not have access to it.");
    let message = format!("model stream failed before completion: {details}");
    Some(if first_event && model_rejected {
        fw::ModelError::Unavailable(message)
    } else {
        fw::ModelError::Indeterminate(message)
    })
}

/// Stable typed error carrier for legacy host ports. The invoker decodes the
/// SDK error before applying recovery; host heuristics must not reinterpret it.
pub fn encode_model_error(error: &fw::ModelError) -> String {
    format!(
        "SDK_MODEL_ERROR:{}",
        serde_json::to_string(error).expect("ModelError serialization")
    )
}
pub fn decode_model_error(value: &str) -> Option<fw::ModelError> {
    value
        .strip_prefix("SDK_MODEL_ERROR:")
        .and_then(|json| serde_json::from_str(json).ok())
}
