use async_trait::async_trait;
use eliteforge_agent_core::{ModelError, ProviderErrorCategory, ProviderModelError};
pub use eliteforge_agent_official_plugin_model_genai::{GenaiHttpClient, ModelHttpClientOptions};
use serde_json::Value;
use std::sync::Arc;

pub type EventSink<'a> = Arc<dyn Fn(Value) -> Result<(), ModelError> + Send + Sync + 'a>;

/// A transport makes exactly one HTTP request. Authentication recovery is an
/// explicit invoker action, never a transport retry.
#[async_trait]
pub trait ModelTransport: Send + Sync {
    /// Native protocol execution must be provided explicitly by the injected
    /// transport. Never silently replace a host transport with a direct client.
    async fn native_chat(
        &self,
        _: &crate::AgentModelConfig,
        _: &crate::ModelRequest,
        _: Option<Arc<dyn Fn(crate::ModelStreamEvent) + Send + Sync>>,
    ) -> Result<crate::ModelResponse, ModelError> {
        Err(ModelError::Fatal(
            "injected model transport does not support this native protocol".into(),
        ))
    }
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
#[derive(Clone)]
pub struct ModelHttpClient(reqwest::Client, Option<GenaiHttpClient>);
impl std::fmt::Debug for ModelHttpClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ModelHttpClient")
            .field("native_protocols", &self.1.is_some())
            .finish_non_exhaustive()
    }
}
impl ModelHttpClient {
    pub fn new() -> Self {
        Self::from_options(&ModelHttpClientOptions::default())
            .expect("model HTTP client initialization")
    }
    pub fn from_builder(builder: reqwest::ClientBuilder) -> Result<Self, reqwest::Error> {
        builder
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map(|client| Self(client, None))
    }
    pub fn with_native_client(mut self, client: GenaiHttpClient) -> Self {
        self.1 = Some(client);
        self
    }
    pub fn from_options(options: &ModelHttpClientOptions) -> Result<Self, ModelError> {
        let mut builder = reqwest::Client::builder()
            .danger_accept_invalid_certs(options.accept_invalid_certificates);
        if let Some(value) = options.connect_timeout {
            builder = builder.connect_timeout(value);
        }
        if let Some(value) = options.timeout {
            builder = builder.timeout(value);
        }
        if let Some(value) = options.read_timeout {
            builder = builder.read_timeout(value);
        }
        if let Some(value) = options.pool_max_idle_per_host {
            builder = builder.pool_max_idle_per_host(value);
        }
        if let Some(value) = options.pool_idle_timeout {
            builder = builder.pool_idle_timeout(value);
        }
        if let Some(value) = options.tcp_keepalive {
            builder = builder.tcp_keepalive(value);
        }
        if let Some(value) = &options.proxy_url {
            builder = builder.proxy(
                reqwest::Proxy::all(value)
                    .map_err(|_| ModelError::Fatal("invalid model proxy configuration".into()))?,
            );
        }
        Ok(Self::from_builder(builder)
            .map_err(|_| ModelError::Fatal("model HTTP client initialization failed".into()))?
            .with_native_client(GenaiHttpClient::from_options(options)?))
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
    headers: reqwest::header::HeaderMap,
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
            headers: reqwest::header::HeaderMap::new(),
        }
    }
    pub fn with_headers(
        mut self,
        headers: &std::collections::BTreeMap<String, String>,
    ) -> Result<Self, ModelError> {
        self.headers = connection_headers(headers)?;
        Ok(self)
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
        let request = self
            .client
            .0
            .post(format!(
                "{}/{}",
                self.base_url.trim_end_matches('/'),
                path.trim_start_matches('/')
            ))
            .headers(self.headers.clone())
            .json(body);
        Ok(if self.api_key.is_empty() {
            request
        } else {
            request.bearer_auth(&self.api_key)
        })
    }
}

/// Validate user supplied headers without placing their values in diagnostics.
pub fn validate_request_headers(
    headers: &std::collections::BTreeMap<String, String>,
) -> Result<(), ModelError> {
    connection_headers(headers).map(|_| ())
}

fn connection_headers(
    headers: &std::collections::BTreeMap<String, String>,
) -> Result<reqwest::header::HeaderMap, ModelError> {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    if headers.len() > 64
        || headers
            .iter()
            .map(|(name, value)| name.len() + value.len())
            .sum::<usize>()
            > 32 * 1024
    {
        return Err(ModelError::Fatal(
            "model request headers exceed limit".into(),
        ));
    }
    let mut result = HeaderMap::new();
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| ModelError::Fatal("invalid model request header name".into()))?;
        if matches!(
            name.as_str(),
            "host"
                | "content-length"
                | "transfer-encoding"
                | "connection"
                | "authorization"
                | "proxy-authorization"
                | "x-api-key"
                | "x-goog-api-key"
                | "api-key"
                | "cookie"
        ) {
            return Err(ModelError::Fatal("reserved model request header".into()));
        }
        let mut value = HeaderValue::from_str(value)
            .map_err(|_| ModelError::Fatal("invalid model request header value".into()))?;
        value.set_sensitive(true);
        if result.insert(name, value).is_some() {
            return Err(ModelError::Fatal("duplicate model request header".into()));
        }
    }
    Ok(result)
}
#[async_trait]
impl ModelTransport for HttpModelTransport {
    async fn native_chat(
        &self,
        config: &crate::AgentModelConfig,
        request: &crate::ModelRequest,
        sink: Option<Arc<dyn Fn(crate::ModelStreamEvent) + Send + Sync>>,
    ) -> Result<crate::ModelResponse, ModelError> {
        let client = self.client.1.clone().ok_or_else(|| ModelError::Fatal(
            "native protocols require ModelHttpClient::from_options or an explicitly injected native client".into(),
        ))?;
        // The transport owns routing and authorization, even when the request
        // DTO was built before the host selected its concrete transport.
        let mut config = config.clone();
        config.base_url = self.base_url.clone();
        config.api_key = self.api_key.clone();
        config.request_headers = self
            .headers
            .iter()
            .map(|(name, value)| {
                Ok((
                    name.as_str().to_owned(),
                    value
                        .to_str()
                        .map_err(|_| {
                            ModelError::Fatal("invalid model request header value".into())
                        })?
                        .to_owned(),
                ))
            })
            .collect::<Result<_, ModelError>>()?;
        crate::genai_bridge::generate(&config, request, sink, client).await
    }
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
                // Before SSE starts, gateways can return a JSON routing/auth error.
                .header("Accept", "text/event-stream, application/json")
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
            .new_gateway_request(
                reqwest::Method::POST,
                &format!("/{}", path.trim_start_matches('/')),
                &self.context,
            )
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
                // Before SSE starts, gateways can return a JSON routing/auth error.
                .header("Accept", "text/event-stream, application/json")
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
        // Upstream rate limits (for example 1302) are temporary. Only the
        // PersonAI business/gateway quota response has quota semantics.
        let message = details.message.to_ascii_lowercase();
        let rate_limited = matches!(
            details.error_type.as_deref(),
            Some("1302" | "rate_limit_exceeded" | "rate_limit_error")
        ) || message.contains("速率限制")
            || message.contains("请求频率")
            || message.contains("rate limit");
        if (details.http_status == Some(429) || details.business_code == Some(429)) && rate_limited
        {
            details.category = ProviderErrorCategory::Transient;
        } else if details.business_code == Some(429)
            || (details.http_status == Some(429)
                && details.error_type.as_deref() == Some("gateway_error"))
        {
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
    let code = [body.pointer("/error/code"), body.pointer("/error/type")]
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .find(|value| !value.trim().is_empty())
        .unwrap_or("");
    if matches!(code, "context_length_exceeded" | "context_window_exceeded") {
        return ModelError::ContextOverflow("model context exceeded".into());
    }
    let message = [
        body.pointer("/error/message"),
        body.get("message"),
        body.get("msg"),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .find(|value| !value.trim().is_empty())
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
        error_type: (!code.is_empty()).then(|| code.into()),
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
        Ok(Arc::new(
            HttpModelTransport::new(self.client.clone(), &config.base_url, &config.api_key)
                .with_headers(&config.request_headers)?,
        ))
    }
}
