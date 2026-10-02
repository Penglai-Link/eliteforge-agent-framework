use crate::*;
use async_trait::async_trait;
use eliteforge_agent_core as fw;
use std::sync::Arc;

#[async_trait]
pub trait ModelTransportFactory: Send + Sync {
    fn on_failure(&self, _: &AgentModelConfig, _: &fw::ModelError) {}
    /// Opaque account identity, excluding rotating credentials.
    fn authorization_scope(&self, _: &AgentModelConfig) -> Result<Option<String>, fw::ModelError> {
        Ok(None)
    }
    async fn create(
        &self,
        config: &AgentModelConfig,
    ) -> Result<Arc<dyn ModelTransport>, fw::ModelError>;
    async fn recover_authentication(
        &self,
        _: &AgentModelConfig,
        _: &fw::ModelError,
    ) -> Result<bool, fw::ModelError> {
        Ok(false)
    }
}
pub struct ProtocolChatModel {
    pub config: AgentModelConfig,
    pub factory: Arc<dyn ModelTransportFactory>,
    pub streaming: bool,
}
#[async_trait]
impl fw::ChatModel for ProtocolChatModel {
    fn on_failure(&self, _: &fw::Extensions, error: &fw::ModelError) {
        self.factory.on_failure(&self.config, error);
    }
    async fn recover_authentication(
        &self,
        _: &fw::Extensions,
        error: &fw::ModelError,
    ) -> Result<bool, fw::ModelError> {
        self.factory
            .recover_authentication(&self.config, error)
            .await
    }
    async fn generate(
        &self,
        _: &str,
        request: fw::ChatRequest,
        stream: &dyn fw::ChatStream,
    ) -> Result<fw::ChatResponse, fw::ModelError> {
        let input = request_from_framework(&request)?;
        let mut config = self.config.clone();
        config.temperature = request.options.temperature.or(config.temperature);
        config.top_p = request.options.top_p.or(config.top_p);
        config.max_tokens = request.options.max_output_tokens;
        let client = HttpProtocolClient::new(self.factory.create(&self.config).await?);
        let response = if self.streaming {
            let (tx, mut rx) = tokio::sync::mpsc::channel(256);
            let overflow = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let callback_overflow = overflow.clone();
            let operation = client.generate_stream(
                &config,
                &input,
                Arc::new(move |event| {
                    if tx.try_send(event).is_err() {
                        callback_overflow.store(true, std::sync::atomic::Ordering::Release);
                    }
                }),
            );
            tokio::pin!(operation);
            let result = loop {
                if overflow.load(std::sync::atomic::Ordering::Acquire) {
                    return Err(fw::ModelError::Fatal(
                        "model stream handoff overflow".into(),
                    ));
                }
                tokio::select! { biased;
                    event = rx.recv() => if let Some(event) = event { emit(stream, event).await; } else { break operation.await; },
                    result = &mut operation => break result,
                }
            };
            while let Ok(event) = rx.try_recv() {
                emit(stream, event).await;
            }
            if overflow.load(std::sync::atomic::Ordering::Acquire) {
                return Err(fw::ModelError::Fatal(
                    "model stream handoff overflow".into(),
                ));
            }
            result?
        } else {
            client.generate(&config, &input).await?
        };
        response_to_framework(response)
    }
}
async fn emit(stream: &dyn fw::ChatStream, event: ModelStreamEvent) {
    match event.kind {
        ModelStreamEventKind::Text => stream.text_delta(&event.delta).await,
        _ => stream.reasoning_delta(&event.delta).await,
    }
}
#[derive(Clone)]
struct ProtocolMetadataWitness(serde_json::Value);
pub fn request_to_framework(request: ModelRequest) -> Result<fw::ChatRequest, fw::ModelError> {
    let mut messages =
        crate::messages::encode_history(&request.messages).map_err(fw::ModelError::Fatal)?;
    let mut protocol_metadata = None;
    // Native continuation is model input too. Keep it in a counted public
    // content block, rather than hidden unbudgeted extension data.
    if request.previous_response_id.is_some()
        || request.response_input_items.is_some()
        || request.system_agent_session_id.is_some()
    {
        let value = serde_json::json!({"wireProtocolMetadata":{"systemAgentSessionId":request.system_agent_session_id,"previousResponseId":request.previous_response_id,"responseInputItems":request.response_input_items}});
        protocol_metadata = Some(value.clone());
        messages.push(fw::Message::user(fw::Content(vec![
            fw::ContentBlock::Json { value },
        ])));
    }
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            fw::ToolDescriptor::new(
                &tool.tool_name,
                &tool.tool_name,
                &tool.description,
                tool.input_schema.clone(),
            )
        })
        .collect();
    let mut extensions = fw::Extensions::default();
    if let Some(metadata) = protocol_metadata {
        extensions.insert(ProtocolMetadataWitness(metadata));
    }
    Ok(fw::ChatRequest {
        messages,
        tools,
        extensions,
        options: Default::default(),
    })
}
pub fn response_to_framework(response: ModelResponse) -> Result<fw::ChatResponse, fw::ModelError> {
    let usage = response.token_usage.unwrap_or_default();
    let response_metrics = response.token_usage.map(|usage| fw::ProviderMetrics {
        cached_input_tokens: usage.cached_input_tokens.map(u64::from),
        reasoning_tokens: usage.reasoning_tokens.map(u64::from),
    });
    let result = fw::ChatResponse {
        content: response.output_text().into(),
        structured: serde_json::from_str(&response.output_text()).ok(),
        tool_calls: response
            .tool_calls()
            .into_iter()
            .map(|call| {
                fw::ToolCall::new(call.call_id.unwrap_or_default(), call.tool_name, call.input)
            })
            .collect(),
        usage: fw::Usage {
            input_tokens: usage.input_tokens.into(),
            output_tokens: usage.output_tokens.into(),
        },
        model_context: Some(fw::ModelMessageContext {
            model: fw::ModelRef::new("wire", "response"),
            protocol: "http".into(),
            format: "wire.response.v1".into(),
            data: serde_json::to_value(&response)
                .map_err(|_| fw::ModelError::Fatal("could not encode model response".into()))?,
        }),
    };
    Ok(match response_metrics {
        Some(metrics) => result.with_provider_metrics(metrics),
        None => result,
    })
}
pub fn response_from_framework(
    response: fw::ChatResponse,
) -> Result<ModelResponse, fw::ModelError> {
    serde_json::from_value(
        response
            .model_context
            .filter(|context| context.format == "wire.response.v1")
            .map(|context| context.data)
            .or(response.structured)
            .ok_or_else(|| fw::ModelError::Fatal("protocol response missing".into()))?,
    )
    .map_err(|_| fw::ModelError::Fatal("protocol response invalid".into()))
}

pub fn request_from_framework(request: &fw::ChatRequest) -> Result<ModelRequest, fw::ModelError> {
    let mut input = ModelRequest {
        messages: Vec::new(),
        tools: request
            .tools
            .iter()
            .map(|tool| EffectiveTool {
                tool_name: tool.call_name.clone(),
                description: tool.description.clone(),
                input_schema: tool.input_schema.clone(),
            })
            .collect(),
        prepared_tool_schema: None,
        system_agent_session_id: None,
        response_input_items: None,
        previous_response_id: None,
    };
    for message in &request.messages {
        if let [fw::ContentBlock::Json { value }] = message.content.0.as_slice()
            && message.role == fw::Role::User
            && request
                .extensions
                .get::<ProtocolMetadataWitness>()
                .is_some_and(|witness| witness.0 == *value)
            && let Some(metadata) = value.get("wireProtocolMetadata")
        {
            input.system_agent_session_id =
                serde_json::from_value(metadata["systemAgentSessionId"].clone())
                    .map_err(|_| fw::ModelError::Fatal("invalid protocol metadata".into()))?;
            input.previous_response_id =
                serde_json::from_value(metadata["previousResponseId"].clone())
                    .map_err(|_| fw::ModelError::Fatal("invalid protocol metadata".into()))?;
            input.response_input_items =
                serde_json::from_value(metadata["responseInputItems"].clone())
                    .map_err(|_| fw::ModelError::Fatal("invalid protocol metadata".into()))?;
        } else {
            input
                .messages
                .push(crate::messages::decode(message).map_err(fw::ModelError::Fatal)?);
        }
    }
    Ok(input)
}

/// Captures authorization at binding time, before an account can change while
/// a run waits for context/tools. Credential rotation within that scope remains allowed.
pub struct ScopedModelTransportFactory {
    inner: Arc<dyn ModelTransportFactory>,
    scope: Option<String>,
}
impl ScopedModelTransportFactory {
    pub fn new(
        inner: Arc<dyn ModelTransportFactory>,
        config: &AgentModelConfig,
    ) -> Result<Self, fw::ModelError> {
        let scope = inner.authorization_scope(config)?;
        Ok(Self { inner, scope })
    }
    fn ensure(&self, config: &AgentModelConfig) -> Result<(), fw::ModelError> {
        if self.inner.authorization_scope(config)? == self.scope {
            return Ok(());
        }
        Err(fw::ModelError::Provider(Box::new(fw::ProviderModelError {
            authorization_attempt: None,
            provider: "scoped-model".into(),
            category: fw::ProviderErrorCategory::AccountChanged,
            http_status: None,
            business_code: None,
            error_type: None,
            model: None,
            message: "model authorization changed after binding".into(),
        })))
    }
}
#[async_trait]
impl ModelTransportFactory for ScopedModelTransportFactory {
    fn authorization_scope(
        &self,
        config: &AgentModelConfig,
    ) -> Result<Option<String>, fw::ModelError> {
        self.ensure(config)?;
        Ok(self.scope.clone())
    }
    fn on_failure(&self, config: &AgentModelConfig, error: &fw::ModelError) {
        self.inner.on_failure(config, error)
    }
    async fn create(
        &self,
        config: &AgentModelConfig,
    ) -> Result<Arc<dyn ModelTransport>, fw::ModelError> {
        self.ensure(config)?;
        let transport = self.inner.create(config).await?;
        self.ensure(config)?;
        Ok(transport)
    }
    async fn recover_authentication(
        &self,
        config: &AgentModelConfig,
        error: &fw::ModelError,
    ) -> Result<bool, fw::ModelError> {
        self.ensure(config)?;
        let recovered = self.inner.recover_authentication(config, error).await?;
        self.ensure(config)?;
        Ok(recovered)
    }
}
/// Host configuration supplies the declared window and output ceiling. Missing
/// provider capability/window data stays unknown instead of inventing limits.
pub fn model_descriptor(config: &AgentModelConfig) -> fw::ModelDescriptor {
    let mut descriptor = fw::ModelDescriptor::new(&config.model_code)
        .with_input_modalities([fw::Modality::Text, fw::Modality::Image]);
    descriptor.context_window_tokens = config.context_window_tokens.map(|v| v as usize);
    descriptor.max_output_tokens = config.max_tokens.filter(|v| *v > 0);
    descriptor
}
pub fn model_protocol(config: &AgentModelConfig) -> &'static str {
    match config.protocol {
        AgentModelProtocol::Responses => "openai.responses",
        AgentModelProtocol::ChatCompletions => "openai.chat-completions",
        AgentModelProtocol::Embeddings => "openai.embeddings",
    }
}
