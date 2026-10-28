//! Legacy host DTO bridge to the shared protocol adapters. It owns no provider
//! catalogue, product credentials, retry loop, or model-name inference.
use crate::*;
use async_trait::async_trait;
use eliteforge_agent_core as fw;
use eliteforge_agent_official_plugin_model_genai::{
    AdapterKind, AuthData, ChatOptions, GenaiChatAdapter,
};
use std::sync::Arc;

pub(crate) const CONTINUATION: &str = "framework_genai_continuation_v1";

pub(crate) fn supported(protocol: AgentModelProtocol) -> bool {
    matches!(
        protocol,
        AgentModelProtocol::Anthropic | AgentModelProtocol::Gemini
    )
}

pub(crate) async fn generate(
    config: &AgentModelConfig,
    request: &ModelRequest,
    sink: Option<Arc<dyn Fn(ModelStreamEvent) + Send + Sync>>,
    client: GenaiHttpClient,
) -> Result<ModelResponse, fw::ModelError> {
    validate_request_headers(&config.request_headers)?;
    let adapter = match config.protocol {
        AgentModelProtocol::Anthropic => AdapterKind::Anthropic,
        AgentModelProtocol::Gemini => AdapterKind::Gemini,
        _ => return Err(fw::ModelError::Fatal("unsupported genai protocol".into())),
    };
    let endpoint = reqwest::Url::parse(&config.base_url)
        .map_err(|_| fw::ModelError::Fatal("invalid model endpoint".into()))?;
    if !matches!(endpoint.scheme(), "https" | "http")
        || endpoint.host_str().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(fw::ModelError::Fatal("invalid model endpoint".into()));
    }
    if config
        .api_path
        .as_deref()
        .is_some_and(|path| !path.trim().is_empty())
    {
        return Err(fw::ModelError::Fatal(
            "this protocol uses its API base URL; custom request paths are unsupported".into(),
        ));
    }
    if config.responses_stateful_enabled
        || request.previous_response_id.is_some()
        || request.response_input_items.is_some()
    {
        return Err(fw::ModelError::Fatal(
            "Responses continuation cannot be used with this protocol".into(),
        ));
    }
    let mut options = ChatOptions::default();
    if !config.thinking_enabled {
        options = serde_json::from_value(serde_json::json!({"reasoning_effort": "Zero"}))
            .map_err(|_| fw::ModelError::Fatal("invalid reasoning configuration".into()))?;
    } else if let Some(effort) = config.reasoning_effort {
        options = serde_json::from_value(serde_json::json!({"reasoning_effort": effort})).map_err(
            |_| fw::ModelError::Fatal("unsupported reasoning effort for this protocol".into()),
        )?;
    }
    if config.thinking_enabled
        && config.reasoning_effort.is_none()
        && [
            "-none", "-zero", "-minimal", "-low", "-medium", "-high", "-xhigh", "-max",
        ]
        .iter()
        .any(|suffix| config.model_name.ends_with(suffix))
    {
        return Err(fw::ModelError::Fatal("this model ID requires an explicit reasoning effort to prevent upstream model-name inference".into()));
    }
    options = options.with_extra_headers(
        config
            .request_headers
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<Vec<_>>(),
    );
    // The stable connection/model pair scopes signed provider continuation. The
    // endpoint participates so a saved turn cannot be replayed to a new server.
    use sha2::{Digest, Sha256};
    let routing = serde_json::to_vec(&(&config.request_headers, &config.api_key))
        .map_err(|_| fw::ModelError::Fatal("invalid connection identity".into()))?;
    let routing = format!("{:x}", Sha256::digest(routing));
    let connection_id = format!(
        "{}:{}:{}:{}",
        model_protocol(config),
        config.model_code,
        config.base_url.trim_end_matches('/'),
        routing
    );
    let adapter = GenaiChatAdapter::new(
        connection_id,
        adapter,
        &config.base_url,
        AuthData::from_single(config.api_key.clone()),
    )?
    .with_http_client(client)
    .with_options(options);
    let mut input = request_to_framework(ModelRequest {
        system_agent_session_id: None,
        previous_response_id: None,
        response_input_items: None,
        ..request.clone()
    })?;
    for message in &mut input.messages {
        normalize_message(message)?;
    }
    input.options.temperature = config.temperature;
    input.options.top_p = config.top_p;
    input.options.max_output_tokens = config.max_tokens;
    let result = adapter
        .generate_with_usage(&config.model_name, input, &Sink(sink))
        .await?;
    let usage = result.token_usage.and_then(|usage| {
        let input_tokens = usage.input_tokens?;
        let output_tokens = usage.output_tokens?;
        Some(ModelTokenUsage {
            input_tokens,
            output_tokens,
            total_tokens: usage
                .total_tokens
                .unwrap_or_else(|| input_tokens.saturating_add(output_tokens)),
            cached_input_tokens: usage.cached_input_tokens,
            reasoning_tokens: usage.reasoning_tokens,
        })
    });
    let response = result.response;
    let finish = if response.tool_calls.is_empty() {
        ModelFinish::Final
    } else {
        ModelFinish::ToolCalls
    };
    let mut message = fw::Message::assistant(response.content);
    message.tool_calls = response.tool_calls;
    let mut assistant = crate::messages::decode(&message).map_err(fw::ModelError::Fatal)?;
    if let Some(reasoning) = response.reasoning.filter(|text| !text.is_empty()) {
        assistant
            .parts
            .push(AgentMessagePart::Reasoning { text: reasoning });
    }
    if let Some(continuation) = response.model_context {
        assistant.parts.push(AgentMessagePart::Context {
            kind: CONTINUATION.into(),
            text: String::new(),
            metadata: Some(
                serde_json::to_value(continuation)
                    .map_err(|_| fw::ModelError::Fatal("invalid model continuation".into()))?,
            ),
        });
    }
    Ok(ModelResponse::from_assistant(
        assistant, finish, usage, None, None,
    ))
}

fn normalize_message(message: &mut fw::Message) -> Result<(), fw::ModelError> {
    let mut content = Vec::new();
    for block in std::mem::take(&mut message.content.0) {
        match block {
            fw::ContentBlock::Json { value } if value.get("wireMessagePart").is_some() => {
                let part: AgentMessagePart =
                    serde_json::from_value(value["wireMessagePart"].clone())
                        .map_err(|_| fw::ModelError::Fatal("invalid model message part".into()))?;
                let text = match part {
                    AgentMessagePart::Context { kind, metadata, .. } if kind == CONTINUATION => {
                        if message.role != fw::Role::Assistant || message.model_context.is_some() {
                            return Err(fw::ModelError::Fatal(
                                "invalid model continuation role".into(),
                            ));
                        }
                        message.model_context = Some(
                            serde_json::from_value(metadata.ok_or_else(|| {
                                fw::ModelError::Fatal("missing model continuation".into())
                            })?)
                            .map_err(|_| {
                                fw::ModelError::Fatal("invalid model continuation".into())
                            })?,
                        );
                        continue;
                    }
                    AgentMessagePart::Context { text, .. } => text,
                    AgentMessagePart::ToolResult { output, .. } => output,
                    AgentMessagePart::CompactionCheckpoint {
                        summary, recent, ..
                    } => format!("{summary}\n\n{recent}"),
                    // Tool calls are already represented by top-level identity.
                    // Signed reasoning lives in the opaque continuation only.
                    AgentMessagePart::ToolCall { .. }
                    | AgentMessagePart::Reasoning { .. }
                    | AgentMessagePart::ReasoningSummary { .. } => continue,
                    AgentMessagePart::Text { text } => text,
                    AgentMessagePart::Image { .. } => {
                        return Err(fw::ModelError::Fatal("invalid model image boundary".into()));
                    }
                };
                if !text.is_empty() {
                    content.push(fw::ContentBlock::Text { text });
                }
            }
            fw::ContentBlock::Media { mut media } => {
                if let fw::MediaSource::Base64(value) = &media.source {
                    if value.starts_with("https://") || value.starts_with("http://") {
                        media.source = fw::MediaSource::Url(value.clone());
                    } else if let Some((_, data)) = value
                        .strip_prefix("data:")
                        .and_then(|value| value.split_once(";base64,"))
                    {
                        media.source = fw::MediaSource::Base64(data.into());
                    }
                }
                content.push(fw::ContentBlock::Media { media });
            }
            block => content.push(block),
        }
    }
    message.content.0 = content;
    Ok(())
}

struct Sink(Option<Arc<dyn Fn(ModelStreamEvent) + Send + Sync>>);
#[async_trait]
impl fw::ChatStream for Sink {
    async fn text_delta(&self, delta: &str) {
        if let Some(sink) = &self.0 {
            sink(ModelStreamEvent::text_delta(delta));
        }
    }
    async fn reasoning_delta(&self, delta: &str) {
        if let Some(sink) = &self.0 {
            sink(ModelStreamEvent::reasoning_delta(delta));
        }
    }
}
