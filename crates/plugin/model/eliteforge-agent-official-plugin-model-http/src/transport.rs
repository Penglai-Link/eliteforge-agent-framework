use async_trait::async_trait;
use eliteforge_agent_core::{ModelError, ProviderErrorCategory, ProviderModelError};
use serde_json::Value;
use std::sync::Arc;

pub type EventSink<'a> = Arc<dyn Fn(Value) -> Result<(), ModelError> + Send + Sync + 'a>;

/// A transport makes exactly one HTTP request. Authentication recovery is an
/// explicit invoker action, never a transport retry.
#[async_trait]
pub trait ModelTransport: Send + Sync {
    async fn json(&self, path: &str, body: &Value) -> Result<Value, ModelError>;
    /// Status-only connectivity probe, with bounded response drainage.
    async fn probe(&self, path: &str, body: &Value) -> Result<Value, ModelError> {
        self.json(path, body).await
    }
    async fn events(&self, path: &str, body: &Value, sink: EventSink<'_>)
    -> Result<(), ModelError>;
    async fn recover_authentication(&self) -> Result<bool, ModelError> {
        Ok(false)
    }
}

/// Model HTTP pool whose redirect policy cannot hide a second model request.
/// Host builders retain proxy, connection, pool and timeout settings.
#[derive(Clone, Debug)]
pub struct ModelHttpClient(reqwest::Client);
impl ModelHttpClient {
    pub fn new() -> Self {
        Self::from_builder(reqwest::Client::builder()).expect("model HTTP client initialization")
    }
    pub fn from_builder(builder: reqwest::ClientBuilder) -> Result<Self, reqwest::Error> {
        builder
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map(Self)
    }
}
impl Default for ModelHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

pub struct HttpModelTransport {
    client: ModelHttpClient,
    base_url: String,
    api_key: String,
}
impl HttpModelTransport {
    pub fn new(
        client: ModelHttpClient,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            client,
            base_url: base_url.into(),
            api_key: api_key.into(),
        }
    }
    fn request(&self, path: &str, body: &Value) -> Result<reqwest::RequestBuilder, ModelError> {
        if self.base_url.trim().is_empty() || path.trim_matches('/').is_empty() {
            return Err(ModelError::Fatal("model endpoint is required".into()));
        }
        if serde_json::to_vec(body)
            .map_err(|_| ModelError::Fatal("invalid model body".into()))?
            .len()
            > 16 * 1024 * 1024
        {
            return Err(ModelError::Fatal("model request exceeds byte limit".into()));
        }
        Ok(self
            .client
            .0
            .post(format!(
                "{}/{}",
                self.base_url.trim_end_matches('/'),
                path.trim_start_matches('/')
            ))
            .bearer_auth(&self.api_key)
            .json(body))
    }
}
#[async_trait]
impl ModelTransport for HttpModelTransport {
    async fn json(&self, path: &str, body: &Value) -> Result<Value, ModelError> {
        read_json(
            self.request(path, body)?
                .send()
                .await
                .map_err(transport_error)?,
        )
        .await
    }
    async fn probe(&self, path: &str, body: &Value) -> Result<Value, ModelError> {
        let response = require_success(
            self.request(path, body)?
                .send()
                .await
                .map_err(transport_error)?,
        )
        .await?;
        let bytes = read_bytes(response).await?;
        Ok(serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }
    async fn events(
        &self,
        path: &str,
        body: &Value,
        sink: EventSink<'_>,
    ) -> Result<(), ModelError> {
        read_events(
            self.request(path, body)?
                .header("Accept", "text/event-stream")
                .send()
                .await
                .map_err(transport_error)?,
            sink,
        )
        .await
    }
}

/// Product identity, account credentials and business route are host supplied.
/// The official PersonAI SDK constructs/encrypts the request; Framework sends
/// it once and uses the same bounded parser as all other model transports.
pub struct PersonAiModelTransport {
    api: crab_serverapi::AuthorizedClientApi,
    context: crab_serverapi::AuthorizedClientContext,
    quality: crab_serverapi::gateway::QualityType,
    conversation: Option<String>,
}
impl PersonAiModelTransport {
    pub fn new(
        base_url: impl Into<String>,
        context: crab_serverapi::AuthorizedClientContext,
        quality: crab_serverapi::gateway::QualityType,
        conversation: Option<String>,
    ) -> Result<Self, ModelError> {
        Ok(Self {
            api: crab_serverapi::AuthorizedClientApi::with_client(
                base_url,
                ModelHttpClient::from_builder(
                    reqwest::Client::builder()
                        .danger_accept_invalid_certs(true)
                        .connect_timeout(std::time::Duration::from_secs(10)),
                )
                .map_err(|_| {
                    ModelError::Fatal("PersonAI HTTP client initialization failed".into())
                })?
                .0,
            ),
            context,
            quality,
            conversation,
        })
    }
    fn request(&self, path: &str, body: &Value) -> Result<reqwest::RequestBuilder, ModelError> {
        use crab_serverapi::crypto::ClientEnvelopeEncryptor;
        let mut body = body.clone();
        if path.trim_matches('/') == "chat/completions" {
            if body.get("quality_type").is_none() {
                body["quality_type"] = Value::String(self.quality.as_str().into());
            }
            if let Some(id) = &self.conversation
                && body.get("conversation_id").is_none()
            {
                body["conversation_id"] = Value::String(id.clone());
            }
        }
        if serde_json::to_vec(&body)
            .map_err(|_| ModelError::Fatal("invalid model body".into()))?
            .len()
            > 16 * 1024 * 1024
        {
            return Err(ModelError::Fatal("model request exceeds byte limit".into()));
        }
        let envelope =
            ClientEnvelopeEncryptor::encrypt_with_context(&self.context, &body.to_string())
                .map_err(|_| ModelError::Fatal("PersonAI request encryption failed".into()))?;
        Ok(self
            .api
            .new_gateway_request(reqwest::Method::POST, path, &self.context)
            .map_err(|_| ModelError::Fatal("PersonAI request authorization failed".into()))?
            .json(&envelope))
    }
}
#[async_trait]
impl ModelTransport for PersonAiModelTransport {
    async fn json(&self, path: &str, body: &Value) -> Result<Value, ModelError> {
        read_json(
            self.request(path, body)?
                .send()
                .await
                .map_err(transport_error)?,
        )
        .await
        .map_err(person_ai_error)
    }
    async fn probe(&self, path: &str, body: &Value) -> Result<Value, ModelError> {
        let response = require_success(
            self.request(path, body)?
                .send()
                .await
                .map_err(transport_error)?,
        )
        .await
        .map_err(person_ai_error)?;
        let bytes = read_bytes(response).await?;
        Ok(serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }
    async fn events(
        &self,
        path: &str,
        body: &Value,
        sink: EventSink<'_>,
    ) -> Result<(), ModelError> {
        read_events(
            self.request(path, body)?
                .header("Accept", "text/event-stream")
                .send()
                .await
                .map_err(transport_error)?,
            sink,
        )
        .await
        .map_err(person_ai_error)
    }
}

fn person_ai_error(mut error: ModelError) -> ModelError {
    if let ModelError::Provider(ref mut details) = error {
        details.provider = "personai".into();
        if details.http_status == Some(429) || details.business_code == Some(429) {
            details.category = ProviderErrorCategory::Quota;
        } else if details.http_status == Some(404) {
            details.category = ProviderErrorCategory::Unavailable;
        }
    }
    error
}

fn transport_error(error: reqwest::Error) -> ModelError {
    if error.is_connect() {
        ModelError::Transient("model connection unavailable".into())
    } else {
        ModelError::Indeterminate("model transport ended before a complete response".into())
    }
}
pub fn status_error(status: u16, body: &Value) -> ModelError {
    let code = body
        .pointer("/error/code")
        .or_else(|| body.pointer("/error/type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if matches!(code, "context_length_exceeded" | "context_window_exceeded") {
        return ModelError::ContextOverflow("model context exceeded".into());
    }
    let message = body
        .pointer("/error/message")
        .or_else(|| body.get("message"))
        .or_else(|| body.get("msg"))
        .and_then(Value::as_str)
        .unwrap_or("model request failed")
        .to_string();
    let code = if message.contains("device_unbound") || message.contains("设备已解绑") {
        "device_unbound"
    } else {
        code
    };
    let category = match (status, code) {
        (_, "device_unbound" | "decrypt_error" | "DecryptFailed") => {
            ProviderErrorCategory::AccountChanged
        }
        (429, _)
            if message.contains("积分")
                || message.to_ascii_lowercase().contains("quota")
                || message.to_ascii_lowercase().contains("balance") =>
        {
            ProviderErrorCategory::Quota
        }
        (404, "gateway_error" | "model_not_found") => ProviderErrorCategory::Unavailable,

        (401, _) => ProviderErrorCategory::Authentication,
        (403, _) => ProviderErrorCategory::Permission,
        (429, _)
            if matches!(
                code,
                "insufficient_quota" | "insufficient_balance" | "points_exhausted"
            ) =>
        {
            ProviderErrorCategory::Quota
        }
        (408 | 429 | 500..=599, _) => ProviderErrorCategory::Transient,
        _ => ProviderErrorCategory::Other,
    };
    ModelError::Provider(Box::new(ProviderModelError {
        authorization_attempt: None,
        provider: "http".into(),
        category,
        http_status: Some(status),
        business_code: body.get("code").and_then(Value::as_i64).map(|n| n as i32),
        error_type: Some(code.into()),
        model: None,
        message,
    }))
}
async fn require_success(response: reqwest::Response) -> Result<reqwest::Response, ModelError> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status().as_u16();
    let bytes = read_bytes(response).await?;
    let body = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| serde_json::json!({"message":String::from_utf8_lossy(&bytes)}));
    Err(status_error(status, &body))
}
async fn read_json(response: reqwest::Response) -> Result<Value, ModelError> {
    read_body(require_success(response).await?).await
}
async fn read_events(response: reqwest::Response, sink: EventSink<'_>) -> Result<(), ModelError> {
    let mut response = require_success(response).await?;
    let mut decoder = SseDecoder::new(256 * 1024);
    while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
        decoder.feed(&chunk, &sink)?;
    }
    decoder.finish(&sink)
}

/// Byte-oriented, bounded SSE decoder; arbitrary UTF-8 chunk boundaries and
/// LF/CRLF framing are handled before strict decoding. Multiline data is one
/// JSON event, as required by SSE.
pub struct SseDecoder {
    buffer: Vec<u8>,
    frame: Vec<u8>,
    limit: usize,
}
impl SseDecoder {
    pub fn new(limit: usize) -> Self {
        Self {
            buffer: Vec::new(),
            frame: Vec::new(),
            limit,
        }
    }
    pub fn feed(&mut self, bytes: &[u8], sink: &EventSink<'_>) -> Result<(), ModelError> {
        for &byte in bytes {
            if self.buffer.len() + self.frame.len() >= self.limit {
                return Err(ModelError::Fatal(
                    "model SSE frame exceeds byte limit".into(),
                ));
            }
            self.buffer.push(byte);
            if byte == b'\n' {
                let mut line = std::mem::take(&mut self.buffer);
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                if line.is_empty() {
                    self.emit(sink)?;
                } else if let Some(data) = line.strip_prefix(b"data:") {
                    let data = data.strip_prefix(b" ").unwrap_or(data);
                    if !self.frame.is_empty() {
                        self.frame.push(b'\n');
                    }
                    self.frame.extend_from_slice(data);
                }
            }
        }
        Ok(())
    }
    fn emit(&mut self, sink: &EventSink<'_>) -> Result<(), ModelError> {
        let frame = std::mem::take(&mut self.frame);
        if frame.is_empty() {
            return Ok(());
        }
        let data = std::str::from_utf8(&frame)
            .map_err(|_| ModelError::Fatal("model SSE contains invalid UTF-8".into()))?
            .trim();
        if data.is_empty() || data == "[DONE]" {
            return Ok(());
        }
        let event = serde_json::from_str(data)
            .map_err(|_| ModelError::Fatal("model SSE contains invalid JSON".into()))?;
        sink(event)
    }
    pub fn finish(&mut self, sink: &EventSink<'_>) -> Result<(), ModelError> {
        if !self.buffer.is_empty() || !self.frame.is_empty() {
            return Err(ModelError::Indeterminate(
                "model SSE ended inside a frame".into(),
            ));
        }
        self.emit(sink)
    }
}

/// An immutable host-authorized transport; useful for credential verification
/// before those credentials are committed to the product account store.
pub struct FixedModelTransportFactory(pub Arc<dyn ModelTransport>);
#[async_trait]
impl crate::ModelTransportFactory for FixedModelTransportFactory {
    async fn create(
        &self,
        _: &crate::AgentModelConfig,
    ) -> Result<Arc<dyn ModelTransport>, ModelError> {
        Ok(self.0.clone())
    }
}

async fn read_body(response: reqwest::Response) -> Result<Value, ModelError> {
    let bytes = read_bytes(response).await?;
    serde_json::from_slice(&bytes)
        .map_err(|_| ModelError::Indeterminate("invalid model JSON response".into()))
}
async fn read_bytes(mut response: reqwest::Response) -> Result<Vec<u8>, ModelError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
        if bytes.len().saturating_add(chunk.len()) > 16 * 1024 * 1024 {
            return Err(ModelError::Fatal(
                "model response exceeds byte limit".into(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub struct HttpModelTransportFactory {
    client: ModelHttpClient,
}
impl HttpModelTransportFactory {
    pub fn new(client: ModelHttpClient) -> Self {
        Self { client }
    }
}
#[async_trait]
impl crate::ModelTransportFactory for HttpModelTransportFactory {
    async fn create(
        &self,
        config: &crate::AgentModelConfig,
    ) -> Result<Arc<dyn ModelTransport>, ModelError> {
        Ok(Arc::new(HttpModelTransport::new(
            self.client.clone(),
            &config.base_url,
            &config.api_key,
        )))
    }
}
