//! Shared-credential model connections backed by genai 0.7.0-beta.21.
//! Upstream types are confined to this integration crate, not framework contracts.
use async_trait::async_trait;
use eliteforge_agent_core::*;
use futures::StreamExt;
use genai::{
    Client, ModelIden, ServiceTarget,
    chat::{
        ChatMessage, ChatRequest as GenaiChatRequest, ChatStreamEvent, ContentPart, MessageContent,
        ToolResponse,
    },
};
pub use genai::{
    adapter::AdapterKind,
    chat::ChatOptions,
    resolver::{AuthData, Endpoint},
};
use std::{collections::HashMap, sync::Arc};

const FORMAT: &str = "genai/0.7/message";
mod embedding;
mod http_client;
mod plugin;
pub use embedding::GenaiEmbeddingAdapter;
pub use http_client::{GenaiHttpClient, ModelHttpClientOptions};
pub use plugin::{GenaiPlugin, OpenAiChatProtocol};

/// No Debug/Serialize implementation: endpoint, authentication and client stay private.
pub struct GenaiChatAdapter {
    connection_id: String,
    client: Client,
    adapter: AdapterKind,
    endpoint: Endpoint,
    auth: AuthData,
    options: ChatOptions,
}
impl GenaiChatAdapter {
    pub fn new(
        connection_id: impl Into<String>,
        adapter: AdapterKind,
        endpoint: impl Into<String>,
        auth: AuthData,
    ) -> Result<Self, ModelError> {
        let mut endpoint = endpoint.into();
        if !endpoint.ends_with('/') {
            endpoint.push('/');
        }
        Ok(Self {
            connection_id: connection_id.into(),
            client: GenaiHttpClient::from_options(&ModelHttpClientOptions::default())?.0,
            adapter,
            endpoint: Endpoint::from_owned(endpoint),
            auth,
            options: ChatOptions::default(),
        })
    }
    /// Configure transport (timeouts, proxy, TLS) on the supplied genai client.
    /// Calls use an explicit ServiceTarget; no model-name based adapter inference.
    pub fn with_client(mut self, client: Client) -> Self {
        self.client = client;
        self
    }
    pub fn with_http_client(mut self, client: GenaiHttpClient) -> Self {
        self.client = client.0;
        self
    }
    pub fn with_options(mut self, options: ChatOptions) -> Self {
        self.options = options;
        self
    }
    pub fn connection(self: Arc<Self>, models: Vec<ModelDescriptor>) -> ModelConnection {
        ModelConnection::new(self.connection_id.clone(), self.clone(), models)
            .with_protocol(self.protocol())
    }
    pub fn protocol(&self) -> String {
        match self.adapter {
            AdapterKind::OpenAI => "openai/chat-completions",
            AdapterKind::OpenAIResp => "openai/responses",
            AdapterKind::Anthropic => "anthropic/messages",
            AdapterKind::Gemini => "gemini/generate-content",
            AdapterKind::Ollama => "ollama/chat",
            AdapterKind::Cohere => "cohere/chat",
            _ => return format!("genai/{:?}/chat", self.adapter),
        }
        .into()
    }
    fn input_modalities(&self) -> Vec<Modality> {
        match self.adapter {
            AdapterKind::Gemini => vec![
                Modality::Text,
                Modality::Image,
                Modality::Audio,
                Modality::Video,
            ],
            AdapterKind::OpenAI
            | AdapterKind::OpenAIResp
            | AdapterKind::Anthropic
            | AdapterKind::Ollama => vec![Modality::Text, Modality::Image],
            _ => vec![Modality::Text],
        }
    }
}

#[async_trait]
impl ModelDiscovery for GenaiChatAdapter {
    async fn discover(&self) -> Result<Vec<ModelDescriptor>, ModelError> {
        let names = self
            .client
            .all_model_names(self.adapter, (self.endpoint.clone(), self.auth.clone()))
            .await
            .map_err(map_error)?;
        let mut names = names;
        names.sort();
        names.dedup();
        Ok(names.into_iter().map(ModelDescriptor::new).collect())
    }
}

/// Provider-reported counters remain optional for hosts that distinguish unknown
/// usage from an actual zero. Framework's core aggregate is intentionally separate.
#[derive(Debug, Clone, Default)]
pub struct GenaiTokenUsage {
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub total_tokens: Option<u32>,
    pub cached_input_tokens: Option<u32>,
    pub reasoning_tokens: Option<u32>,
}

pub struct GenaiChatOutput {
    pub response: ChatResponse,
    pub token_usage: Option<GenaiTokenUsage>,
}

impl GenaiChatAdapter {
    pub async fn generate_with_usage(
        &self,
        model_id: &str,
        request: ChatRequest,
        stream: &dyn ChatStream,
    ) -> Result<GenaiChatOutput, ModelError> {
        let model = ModelRef::new(&self.connection_id, model_id);
        let generation = request.options.clone();
        if request
            .messages
            .iter()
            .flat_map(|m| m.content.modalities())
            .any(|m| !self.input_modalities().contains(&m))
        {
            return Err(ModelError::Fatal(
                "input modality is unsupported by this genai protocol".into(),
            ));
        }
        let request = convert_request(request, &model, &self.protocol())?;
        let target = ServiceTarget {
            endpoint: self.endpoint.clone(),
            auth: self.auth.clone(),
            model: ModelIden::new(self.adapter, model_id),
        };
        let mut options = self
            .options
            .clone()
            .with_capture_content(true)
            .with_capture_tool_calls(true)
            .with_capture_reasoning_content(true)
            .with_capture_usage(true);
        if let Some(value) = generation.temperature {
            options.temperature = Some(value);
        }
        if let Some(value) = generation.top_p {
            options.top_p = Some(value);
        }
        if let Some(value) = generation.max_output_tokens {
            options.max_tokens = Some(value);
        }
        if let Some(value) = generation.stop_sequences {
            options.stop_sequences = value;
        }
        if let Some(output) = generation.output_schema {
            options.response_format = Some(genai::chat::ChatResponseFormat::JsonSpec(
                genai::chat::JsonSpec::new(output.name, output.schema),
            ));
        }
        let mut response = self
            .client
            .exec_chat_stream(target, request, Some(&options))
            .await
            .map_err(map_error)?;
        let mut progressed = false;
        while let Some(event) = response.stream.next().await {
            let event = event.map_err(|error| {
                if progressed { ModelError::Indeterminate("model stream interrupted after output started".into()) }
                else { map_error(error) }
            })?;
            if !matches!(&event, ChatStreamEvent::Start | ChatStreamEvent::Heartbeat) { progressed = true; }
            match event {
                ChatStreamEvent::Chunk(chunk) => stream.text_delta(&chunk.content).await,
                ChatStreamEvent::ReasoningChunk(chunk) => {
                    stream.reasoning_delta(&chunk.content).await
                }
                ChatStreamEvent::End(end) => {
                    if let Some(reason) = &end.captured_stop_reason
                        && !matches!(
                            reason,
                            genai::chat::StopReason::Completed(_)
                                | genai::chat::StopReason::ToolCall(_)
                                | genai::chat::StopReason::StopSequence(_)
                        )
                    {
                        return Err(ModelError::Fatal(
                            "model output truncated, filtered, or stopped abnormally".into(),
                        ));
                    }
                    let content = end.captured_content.unwrap_or_default();
                    let framework_content = from_native_content(&content)?;
                    let tool_calls: Vec<_> = content
                        .tool_calls()
                        .iter()
                        .map(|call| {
                            ToolCall::new(&call.call_id, &call.fn_name, call.fn_arguments.clone())
                        })
                        .collect();
                    let token_usage = end.captured_usage.as_ref().map(|usage| {
                        let count =
                            |value: Option<i32>| value.and_then(|value| u32::try_from(value).ok());
                        GenaiTokenUsage {
                            input_tokens: count(usage.prompt_tokens),
                            output_tokens: count(usage.completion_tokens),
                            total_tokens: count(usage.total_tokens),
                            cached_input_tokens: count(
                                usage
                                    .prompt_tokens_details
                                    .as_ref()
                                    .and_then(|details| details.cached_tokens),
                            ),
                            reasoning_tokens: count(
                                usage
                                    .completion_tokens_details
                                    .as_ref()
                                    .and_then(|details| details.reasoning_tokens),
                            ),
                        }
                    });
                    let usage = end.captured_usage.unwrap_or_default();
                    // Signed reasoning already lives in ordered content parts in 0.7.
                    // Appending flattened reasoning again would break signature pairing.
                    let contains_reasoning = content.contains_reasoning_content();
                    let mut native = ChatMessage::assistant(content);
                    if !contains_reasoning {
                        native =
                            native.with_reasoning_content(end.captured_reasoning_content.clone());
                    }
                    let response = ChatResponse {
                        public_content: None,
                        content: framework_content,
                        reasoning: end.captured_reasoning_content.clone(),
                        structured: None,
                        tool_calls,
                        usage: Usage {
                            input_tokens: usage.prompt_tokens.unwrap_or(0).max(0) as u64,
                            output_tokens: usage.completion_tokens.unwrap_or(0).max(0) as u64,
                        },
                        model_context: Some(ModelMessageContext {
                            protocol: self.protocol(),
                            model,
                            format: FORMAT.into(),
                            data: serde_json::to_value(native).map_err(|_| {
                                ModelError::Fatal("could not encode model continuation".into())
                            })?,
                        }),
                    };
                    return Ok(GenaiChatOutput {
                        response,
                        token_usage,
                    });
                }
                ChatStreamEvent::ThoughtSignatureChunk(_)
                | ChatStreamEvent::Start
                | ChatStreamEvent::ToolCallChunk(_)
                | ChatStreamEvent::Heartbeat => {}
            }
        }
        Err(ModelError::Indeterminate(
            "model stream ended without terminal event".into(),
        ))
    }
}

#[async_trait]
impl ChatModel for GenaiChatAdapter {
    async fn generate(
        &self,
        model_id: &str,
        request: ChatRequest,
        stream: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        Ok(self
            .generate_with_usage(model_id, request, stream)
            .await?
            .response)
    }
}

fn to_native_content(content: Content) -> Result<MessageContent, ModelError> {
    let mut parts = Vec::new();
    for block in content.0 {
        parts.push(match block {
            ContentBlock::Text { text } => ContentPart::Text(text),
            ContentBlock::Json { value } => ContentPart::Text(value.to_string()),
            ContentBlock::Media { media } => ContentPart::Binary(genai::chat::Binary::new(
                media.mime_type,
                match media.source {
                    MediaSource::Url(url) => genai::chat::BinarySource::Url(url),
                    MediaSource::Base64(data) => genai::chat::BinarySource::Base64(data.into()),
                },
                media.name,
            )),
            ContentBlock::Resource { .. } => {
                return Err(ModelError::Fatal(
                    "resolve resource references before genai model invocation".into(),
                ));
            }
        });
    }
    Ok(MessageContent::from_parts(parts))
}

fn from_native_content(content: &MessageContent) -> Result<Content, ModelError> {
    let mut blocks = Vec::new();
    for part in content.clone().into_parts() {
        match part {
            ContentPart::Text(text) => blocks.push(ContentBlock::Text { text }),
            ContentPart::Binary(binary) => blocks.push(ContentBlock::Media {
                media: MediaContent {
                    mime_type: binary.content_type,
                    name: binary.name,
                    estimated_tokens: None,
                    source: match binary.source {
                        genai::chat::BinarySource::Url(url) => MediaSource::Url(url),
                        genai::chat::BinarySource::Base64(data) => {
                            MediaSource::Base64(data.to_string())
                        }
                    },
                },
            }),
            // Reasoning, signatures and provider-specific parts stay in the
            // opaque, model-scoped continuation, preserving their exact order.
            _ => {}
        }
    }
    Ok(Content(blocks))
}

fn convert_request(
    request: ChatRequest,
    model: &ModelRef,
    protocol: &str,
) -> Result<GenaiChatRequest, ModelError> {
    let mut messages = Vec::new();
    let mut names = HashMap::new();
    for message in request.messages {
        for call in &message.tool_calls {
            names.insert(call.id.clone(), call.call_name.clone());
        }
        let converted = match message.role {
            Role::System => ChatMessage::system(to_native_content(message.content)?),
            Role::User => ChatMessage::user(to_native_content(message.content)?),
            Role::Tool => {
                if message.content.has_media() {
                    return Err(ModelError::Fatal("genai tool responses are text-only; resolve media tool output through an explicit context adapter".into()));
                }
                let id = message
                    .tool_call_id
                    .ok_or_else(|| ModelError::Fatal("tool result missing call ID".into()))?;
                let name = names.remove(&id).ok_or_else(|| {
                    ModelError::Fatal("tool result missing originating call".into())
                })?;
                // Upstream ToolResponse is text-only. Encode typed result data
                // explicitly as JSON, never silently throw away structured/error fields.
                let content = match &message.tool_result {
                    Some(metadata) if metadata.is_error || metadata.structured.is_some() || metadata.result_reference.is_some() => serde_json::json!({
                        "content": message.content, "is_error": metadata.is_error, "structured": metadata.structured,
                        "result_reference": metadata.result_reference,
                    }).to_string(),
                    _ => message.content.protocol_text(),
                };
                ChatMessage::tool(ToolResponse::new(id, content).with_fn_name(name))
            }
            Role::Assistant => {
                let native: Option<ChatMessage> = message
                    .model_context
                    .as_ref()
                    .filter(|data| {
                        data.model == *model && data.protocol == protocol && data.format == FORMAT
                    })
                    .map(|data| {
                        serde_json::from_value(data.data.clone()).map_err(|_| {
                            ModelError::Fatal("invalid model continuation data".into())
                        })
                    })
                    .transpose()?;
                if let Some(native) = native {
                    let native_calls = native.content.tool_calls();
                    // Framework invocation IDs are not part of the signed
                    // provider payload. Compare only the protocol fields.
                    let calls_match = native_calls.len() == message.tool_calls.len()
                        && native_calls
                            .iter()
                            .zip(&message.tool_calls)
                            .all(|(native, call)| {
                                native.call_id == call.id
                                    && native.fn_name == call.call_name
                                    && native.fn_arguments == call.arguments
                            });
                    if from_native_content(&native.content)? != message.content || !calls_match {
                        return Err(ModelError::Fatal(
                            "signed model continuation no longer matches assistant message".into(),
                        ));
                    }
                    native
                } else {
                    let mut content = to_native_content(message.content)?.into_parts();
                    content.extend(message.tool_calls.into_iter().map(|call| {
                        ContentPart::ToolCall(genai::chat::ToolCall {
                            call_id: call.id,
                            fn_name: call.call_name,
                            fn_arguments: call.arguments,
                            thought_signatures: None,
                        })
                    }));
                    ChatMessage::assistant(MessageContent::from_parts(content))
                }
            }
        };
        messages.push(converted);
    }
    let mut result = GenaiChatRequest::new(messages);
    if !request.tools.is_empty() {
        result = result.with_tools(request.tools.into_iter().map(|tool| {
            genai::chat::Tool::new(tool.call_name)
                .with_description(tool.description)
                .with_schema(tool.input_schema)
        }));
    }
    Ok(result)
}

/// Do not forward upstream Display: it can contain request bodies or remote payloads.
fn map_error(error: genai::Error) -> ModelError {
    use genai::{Error as E, webc::Error as W};
    match error {
        E::HttpError { status, body, .. } => http_error(status.as_u16(), &body),
        E::WebModelCall { webc_error, .. } | E::WebAdapterCall { webc_error, .. } => {
            match webc_error {
                W::ResponseFailedStatus { status, body, .. } => http_error(status.as_u16(), &body),
                W::Reqwest(error) if error.is_timeout() || error.is_connect() => {
                    ModelError::Transient("model transport unavailable".into())
                }
                _ => ModelError::Fatal("invalid model HTTP response".into()),
            }
        }
        E::ChatResponse { body, .. } => {
            let kind = body
                .pointer("/error/type")
                .or_else(|| body.get("type"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if matches!(
                kind,
                "overloaded_error" | "rate_limit_error" | "server_error"
            ) {
                ModelError::Transient("model stream temporarily unavailable".into())
            } else if kind == "context_length_exceeded" {
                ModelError::ContextOverflow("model context limit exceeded".into())
            } else {
                ModelError::Fatal("model stream rejected request".into())
            }
        }
        E::WebStream { error, .. } => match error.downcast::<genai::Error>() {
            Ok(error) => map_error(*error),
            Err(_) => ModelError::Transient("model stream transport failed".into()),
        },
        _ => ModelError::Fatal("model protocol or configuration error".into()),
    }
}

fn http_error(status: u16, body: &str) -> ModelError {
    let body: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let code = body
        .pointer("/error/code")
        .or_else(|| body.pointer("/error/type"))
        .and_then(|v| v.as_str());
    if matches!(
        code,
        Some("context_length_exceeded" | "context_window_exceeded")
    ) {
        ModelError::ContextOverflow("model context limit exceeded".into())
    } else if status == 429 || status == 408 || (500..=599).contains(&status) {
        ModelError::Transient(format!("model HTTP {status}"))
    } else {
        ModelError::Fatal(format!("model HTTP {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn continuation_requires_exact_connection_model_protocol_and_format() {
        let model = ModelRef::new("connection", "model");
        let data = ModelMessageContext {
            model: model.clone(),
            protocol: "openai/responses".into(),
            format: FORMAT.into(),
            data: serde_json::json!({"invalid-native-message":true}),
        };
        let request = |data| {
            let mut message = Message::assistant("public answer");
            message.model_context = Some(data);
            ChatRequest {
                messages: vec![message],
                tools: vec![],
                options: Default::default(),
                extensions: Default::default(),
            }
        };
        assert!(convert_request(request(data.clone()), &model, "openai/responses").is_err());
        for other in [
            ModelMessageContext {
                model: ModelRef::new("other", "model"),
                ..data.clone()
            },
            ModelMessageContext {
                model: ModelRef::new("connection", "other"),
                ..data.clone()
            },
            ModelMessageContext {
                protocol: "openai/chat-completions".into(),
                ..data.clone()
            },
            ModelMessageContext {
                format: "unknown-version".into(),
                ..data.clone()
            },
        ] {
            assert!(convert_request(request(other), &model, "openai/responses").is_ok());
        }
    }
}
