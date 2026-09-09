//! MCP tools backed by the official Rust SDK.
//!
//! Connections can be owned by a plugin, or explicitly shared by the host.
//! Remote tool failures remain typed results. Lost responses are indeterminate:
//! this adapter never retries an operation which may have produced side effects.
use async_trait::async_trait;
use eliteforge_agent_core::{
    AgentError, Content, ContentBlock, MediaContent, MediaSource, ResourceReference, RunContext,
    Tool, ToolBinding, ToolDescriptor, ToolError, ToolLease, ToolOutput, ToolProvider,
    ToolProviderService,
};
use eliteforge_agent_core::{Plugin, PluginContext, PluginError};
use http::{HeaderName, HeaderValue};
use rmcp::{
    RoleClient, ServiceExt,
    model::{
        CallToolRequest, CallToolRequestParams, ClientRequest, ListToolsRequest,
        PaginatedRequestParams, ServerResult,
    },
    service::{Peer, PeerRequestOptions, RequestHandle, RunningService},
    transport::{
        StreamableHttpClientTransport, TokioChildProcess,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fmt,
    future::Future,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// Secrets, command arguments and endpoints are deliberately omitted from Debug.
#[derive(Clone)]
pub enum McpTransport {
    Stdio {
        program: PathBuf,
        args: Vec<String>,
        env: HashMap<String, String>,
        current_dir: Option<PathBuf>,
        inherit_env: bool,
    },
    StreamableHttp {
        url: String,
        headers: HashMap<HeaderName, HeaderValue>,
    },
}
impl fmt::Debug for McpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Stdio { .. } => "Stdio { configuration: <private> }",
            Self::StreamableHttp { .. } => "StreamableHttp { configuration: <private> }",
        })
    }
}
#[derive(Debug, Clone)]
pub struct McpConfig {
    /// Stable connection namespace, used in tool IDs and model function names.
    pub namespace: String,
    pub transport: McpTransport,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub cancellation_timeout: Duration,
    pub close_timeout: Duration,
    pub max_pages: usize,
    pub max_tools: usize,
    pub max_result_bytes: usize,
    pub max_concurrent_requests: usize,
}
impl McpConfig {
    pub fn new(namespace: impl Into<String>, transport: McpTransport) -> Self {
        Self {
            namespace: namespace.into(),
            transport,
            connect_timeout: Duration::from_secs(30),
            request_timeout: Duration::from_secs(60),
            cancellation_timeout: Duration::from_secs(2),
            close_timeout: Duration::from_secs(5),
            max_pages: 32,
            max_tools: 1024,
            max_result_bytes: 8 * 1024 * 1024,
            max_concurrent_requests: 16,
        }
    }
    pub fn validate(&self) -> Result<(), McpError> {
        if !valid_name(&self.namespace)
            || self.namespace.len() > 24
            || [
                self.connect_timeout,
                self.request_timeout,
                self.cancellation_timeout,
                self.close_timeout,
            ]
            .iter()
            .any(Duration::is_zero)
            || self.max_pages == 0
            || self.max_tools == 0
            || self.max_result_bytes == 0
            || self.max_concurrent_requests == 0
            || self.max_concurrent_requests > tokio::sync::Semaphore::MAX_PERMITS
        {
            return Err(McpError::Configuration);
        }
        if let McpTransport::StreamableHttp { url, headers } = &self.transport {
            let parsed = reqwest::Url::parse(url).map_err(|_| McpError::Configuration)?;
            if !matches!(parsed.scheme(), "http" | "https")
                || !parsed.username().is_empty()
                || parsed.password().is_some()
                || parsed.fragment().is_some()
                || headers.keys().any(|name| {
                    matches!(
                        name.as_str(),
                        "host"
                            | "content-length"
                            | "content-type"
                            | "accept"
                            | "mcp-session-id"
                            | "mcp-protocol-version"
                    )
                })
            {
                return Err(McpError::Configuration);
            }
        }
        Ok(())
    }
}
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// Transport failures are intentionally sanitized: SDK errors can contain URLs,
/// headers or server-supplied text. Host authentication values are never logged.
#[derive(Debug, Clone, thiserror::Error)]
pub enum McpError {
    #[error("invalid MCP configuration")]
    Configuration,
    #[error("MCP connection failed")]
    Connection,
    #[error("MCP connection is closed")]
    Closed,
    #[error("MCP request cancelled")]
    Cancelled,
    #[error("MCP request timed out")]
    Timeout,
    #[error("MCP request failed")]
    Request,
    #[error("MCP response does not match the requested operation")]
    Protocol,
    #[error("MCP discovery exceeds configured limits or contains a cursor cycle")]
    DiscoveryLimit,
    #[error("MCP discovery contains duplicate or invalid tool names or schemas")]
    InvalidTool,
    #[error("MCP result exceeds configured size limit")]
    ResultLimit,
    #[error("MCP shutdown did not finish within its deadline")]
    Shutdown,
}

struct ClientInner {
    config: McpConfig,
    peer: Peer<RoleClient>,
    running: Mutex<RunningService<RoleClient, ()>>,
    closed: CancellationToken,
    close_started: AtomicBool,
    close_outcome: std::sync::Mutex<Option<Result<(), McpError>>>,
    close_done: tokio::sync::Notify,
    permits: Arc<tokio::sync::Semaphore>,
}
impl Drop for ClientInner {
    fn drop(&mut self) {
        self.closed.cancel();
    }
}
#[derive(Clone)]
pub struct McpClient(Arc<ClientInner>);
impl fmt::Debug for McpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpClient")
            .field("namespace", &self.0.config.namespace)
            .field("closed", &self.is_closed())
            .finish()
    }
}
impl McpClient {
    pub async fn connect(config: McpConfig) -> Result<Self, McpError> {
        config.validate()?;
        let running = match &config.transport {
            McpTransport::Stdio {
                program,
                args,
                env,
                current_dir,
                inherit_env,
            } => {
                let mut cmd = tokio::process::Command::new(program);
                cmd.args(args).kill_on_drop(true);
                if !inherit_env {
                    cmd.env_clear();
                }
                cmd.envs(env);
                if let Some(dir) = current_dir {
                    cmd.current_dir(dir);
                }
                // Never inherit a server's stderr: it may contain credentials.
                let (transport, _) = TokioChildProcess::builder(cmd)
                    .stderr(Stdio::null())
                    .spawn()
                    .map_err(|_| McpError::Connection)?;
                tokio::time::timeout(config.connect_timeout, ().serve(transport))
                    .await
                    .map_err(|_| McpError::Timeout)?
                    .map_err(|_| McpError::Connection)?
            }
            McpTransport::StreamableHttp { url, headers } => {
                let http = reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .connect_timeout(config.connect_timeout)
                    .build()
                    .map_err(|_| McpError::Configuration)?;
                let transport_config = StreamableHttpClientTransportConfig::with_uri(url.clone())
                    .custom_headers(headers.clone())
                    .max_concurrent_requests(config.max_concurrent_requests)
                    .control_request_timeout(config.cancellation_timeout)
                    .max_sse_event_size(config.max_result_bytes)
                    // Reinitialization can replay tool POSTs. Host decides recovery.
                    .reinit_on_expired_session(false);
                let transport = StreamableHttpClientTransport::with_client(http, transport_config);
                tokio::time::timeout(config.connect_timeout, ().serve(transport))
                    .await
                    .map_err(|_| McpError::Timeout)?
                    .map_err(|_| McpError::Connection)?
            }
        };
        let peer = running.peer().clone();
        let permits = Arc::new(tokio::sync::Semaphore::new(config.max_concurrent_requests));
        Ok(Self(Arc::new(ClientInner {
            config,
            peer,
            running: Mutex::new(running),
            closed: CancellationToken::new(),
            close_started: AtomicBool::new(false),
            close_outcome: std::sync::Mutex::new(None),
            close_done: tokio::sync::Notify::new(),
            permits,
        })))
    }
    pub fn is_closed(&self) -> bool {
        self.0.closed.is_cancelled() || self.0.peer.is_transport_closed()
    }
    /// Synchronous fallback. Use `close` to await transport/process cleanup.
    pub fn cancel(&self) {
        self.0.closed.cancel();
        if let Ok(running) = self.0.running.try_lock() {
            running.cancellation_token().cancel();
        }
    }
    /// Closes a shared connection for every clone. Idempotent and bounded.
    pub async fn close(&self) -> Result<(), McpError> {
        self.0.closed.cancel();
        if !self.0.close_started.swap(true, Ordering::AcqRel) {
            let inner = self.0.clone();
            tokio::spawn(async move {
                let result = {
                    let mut running = inner.running.lock().await;
                    match running.close_with_timeout(inner.config.close_timeout).await {
                        Ok(Some(
                            rmcp::service::QuitReason::Closed
                            | rmcp::service::QuitReason::Cancelled,
                        )) => Ok(()),
                        _ => Err(McpError::Shutdown),
                    }
                };
                *inner
                    .close_outcome
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(result);
                inner.close_done.notify_waiters();
            });
        }
        loop {
            let notified = self.0.close_done.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(result) = self
                .0
                .close_outcome
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            {
                return result;
            }
            notified.await;
        }
    }

    async fn request(
        &self,
        request: ClientRequest,
        cancelled: impl Future<Output = ()>,
    ) -> Result<ServerResult, McpError> {
        if self.is_closed() {
            return Err(McpError::Closed);
        }
        tokio::pin!(cancelled);
        let deadline = tokio::time::sleep(self.0.config.request_timeout);
        tokio::pin!(deadline);
        let permit = tokio::select! { biased; _ = &mut cancelled => return Err(McpError::Cancelled), _ = self.0.closed.cancelled() => return Err(McpError::Closed), _ = &mut deadline => return Err(McpError::Timeout), permit = self.0.permits.clone().acquire_owned() => permit.map_err(|_| McpError::Closed)? };
        let handle = tokio::select! { biased; _ = &mut cancelled => return Err(McpError::Cancelled), _ = self.0.closed.cancelled() => return Err(McpError::Closed), _ = &mut deadline => return Err(McpError::Timeout), result = self.0.peer.send_request_with_option(request, PeerRequestOptions::no_options()) => result.map_err(|_| McpError::Request)? };
        let mut guard = RequestGuard {
            handle: Some(handle),
            permit: Some(permit),
            timeout: self.0.config.cancellation_timeout,
        };
        let response = tokio::select! { biased;
            _ = &mut cancelled => Err(McpError::Cancelled),
            _ = self.0.closed.cancelled() => Err(McpError::Closed),
            _ = &mut deadline => Err(McpError::Timeout),
            result = &mut guard.handle.as_mut().expect("request handle exists").rx => {
                guard.handle.take();
                result.map_err(|_| McpError::Request).and_then(|r| r.map_err(|_| McpError::Request))
            }
        };
        if response.is_err() {
            guard.cancel().await;
        }
        response
    }
    pub async fn discover(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<Vec<ToolBinding>, McpError> {
        self.discover_until(cancellation.cancelled()).await
    }
    async fn discover_until(
        &self,
        cancelled: impl Future<Output = ()>,
    ) -> Result<Vec<ToolBinding>, McpError> {
        tokio::pin!(cancelled);
        let mut cursor: Option<String> = None;
        let mut seen_cursors = HashSet::new();
        let mut names = HashSet::new();
        let mut call_names = HashSet::new();
        let mut discovery_bytes = 0usize;
        let mut tools = Vec::new();
        for _ in 0..self.0.config.max_pages {
            let params = cursor
                .take()
                .map(|cursor| PaginatedRequestParams::default().with_cursor(Some(cursor)));
            let response = self
                .request(
                    params
                        .map_or_else(ListToolsRequest::default, ListToolsRequest::with_param)
                        .into(),
                    &mut cancelled,
                )
                .await?;
            let ServerResult::ListToolsResult(page) = response else {
                return Err(McpError::Protocol);
            };
            discovery_bytes = discovery_bytes.saturating_add(
                serde_json::to_vec(&page)
                    .map_err(|_| McpError::Protocol)?
                    .len(),
            );
            if discovery_bytes > self.0.config.max_result_bytes {
                return Err(McpError::DiscoveryLimit);
            }
            if tools.len().saturating_add(page.tools.len()) > self.0.config.max_tools {
                return Err(McpError::DiscoveryLimit);
            }
            for remote in page.tools {
                let name = remote.name.to_string();
                let call_name = model_call_name(&self.0.config.namespace, &name);
                if name.is_empty()
                    || !names.insert(name.clone())
                    || !call_names.insert(call_name.clone())
                {
                    return Err(McpError::InvalidTool);
                }
                let schema = Value::Object((*remote.input_schema).clone());
                eliteforge_agent_core::compile_schema(&schema)
                    .map_err(|_| McpError::InvalidTool)?;
                let descriptor = ToolDescriptor::new(
                    format!("mcp/{}/{}", self.0.config.namespace, name),
                    call_name,
                    remote.description.as_deref().unwrap_or_default(),
                    schema,
                );
                // Server annotations are hints, not authority to grant parallel safety.
                tools.push(ToolBinding::new(
                    descriptor,
                    McpTool {
                        client: self.clone(),
                        name,
                    },
                ));
            }
            match page.next_cursor {
                None => return Ok(tools),
                Some(next)
                    if next.len() <= self.0.config.max_result_bytes
                        && seen_cursors.insert(next.clone()) =>
                {
                    cursor = Some(next)
                }
                Some(_) => return Err(McpError::DiscoveryLimit),
            }
        }
        Err(McpError::DiscoveryLimit)
    }
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        self.call_until(name, arguments, cancellation.cancelled())
            .await
    }
    async fn call_until(
        &self,
        name: &str,
        arguments: Value,
        cancelled: impl Future<Output = ()>,
    ) -> Result<ToolOutput, ToolError> {
        if self.is_closed() {
            return Err(ToolError::Failed(McpError::Closed.to_string()));
        }
        let args = arguments
            .as_object()
            .cloned()
            .ok_or_else(|| ToolError::Failed("MCP tool arguments must be a JSON object".into()))?;
        let request =
            CallToolRequest::new(CallToolRequestParams::new(name.to_owned()).with_arguments(args));
        let response = self
            .request(request.into(), cancelled)
            .await
            .map_err(|e| ToolError::Indeterminate(e.to_string()))?;
        let ServerResult::CallToolResult(result) = response else {
            return Err(ToolError::Indeterminate(McpError::Protocol.to_string()));
        };
        if serde_json::to_vec(&result)
            .map_err(|_| ToolError::Indeterminate(McpError::Protocol.to_string()))?
            .len()
            > self.0.config.max_result_bytes
        {
            return Err(ToolError::Indeterminate(McpError::ResultLimit.to_string()));
        }
        convert_result(result)
    }
}
fn model_call_name(namespace: &str, remote_name: &str) -> String {
    let direct = format!("{namespace}__{remote_name}");
    if valid_name(remote_name) && direct.len() <= 64 {
        direct
    } else {
        // Stable mapping for names (including dots/Unicode/long names) that a
        // model function protocol cannot represent. The original stays on wire.
        let hash = Sha256::digest(remote_name.as_bytes());
        let digest: String = hash[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!("{namespace}__t_{digest}")
    }
}

struct RequestGuard {
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
    handle: Option<RequestHandle<RoleClient>>,
    timeout: Duration,
}
impl RequestGuard {
    fn start_cancel(&mut self) -> Option<tokio::task::JoinHandle<()>> {
        let handle = self.handle.take()?;
        let permit = self.permit.take();
        let timeout = self.timeout;
        let runtime = tokio::runtime::Handle::try_current().ok()?;
        Some(runtime.spawn(async move {
            let _permit = permit;
            let _ = tokio::time::timeout(
                timeout,
                handle.cancel(Some("client operation cancelled".into())),
            )
            .await;
        }))
    }
    async fn cancel(&mut self) {
        if let Some(task) = self.start_cancel() {
            let _ = task.await;
        }
    }
}
impl Drop for RequestGuard {
    fn drop(&mut self) {
        drop(self.start_cancel());
    }
}
struct McpTool {
    client: McpClient,
    name: String,
}
#[async_trait]
impl Tool for McpTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        self.client
            .call_until(&self.name, arguments, context.cancelled())
            .await
    }
}
#[async_trait]
impl ToolProvider for McpClient {
    async fn lease(&self, context: &RunContext) -> Result<ToolLease, AgentError> {
        self.discover_until(context.cancelled())
            .await
            .map(ToolLease::new)
            .map_err(|e| AgentError::ToolSetup(e.to_string()))
    }
}

fn convert_result(result: rmcp::model::CallToolResult) -> Result<ToolOutput, ToolError> {
    let mut blocks = Vec::new();
    for item in result.content {
        match item {
            rmcp::model::ContentBlock::Text(text) => {
                blocks.push(ContentBlock::Text { text: text.text })
            }
            rmcp::model::ContentBlock::Image(image) => blocks.push(ContentBlock::Media {
                media: MediaContent {
                    mime_type: image.mime_type,
                    source: MediaSource::Base64(image.data),
                    name: None,
                    estimated_tokens: None,
                },
            }),
            rmcp::model::ContentBlock::Audio(audio) => blocks.push(ContentBlock::Media {
                media: MediaContent {
                    mime_type: audio.mime_type,
                    source: MediaSource::Base64(audio.data),
                    name: None,
                    estimated_tokens: None,
                },
            }),
            rmcp::model::ContentBlock::ResourceLink(resource) => {
                blocks.push(ContentBlock::Resource {
                    resource: ResourceReference {
                        uri: resource.uri,
                        mime_type: resource.mime_type,
                        name: Some(resource.name),
                        size_bytes: resource.size,
                    },
                })
            }
            rmcp::model::ContentBlock::Resource(resource) => {
                // Keep the embedded envelope intact, including URI, MIME, and blob/text.
                blocks.push(ContentBlock::Json {
                    value: serde_json::to_value(resource)
                        .map_err(|_| ToolError::Indeterminate(McpError::Protocol.to_string()))?,
                });
            }
            _ => {
                return Err(ToolError::Indeterminate(
                    "unsupported MCP content type".into(),
                ));
            }
        }
    }
    Ok(ToolOutput {
        content: Content(blocks),
        structured: result.structured_content,
        is_error: result.is_error.unwrap_or(false),
        ..ToolOutput::default()
    })
}

enum Ownership {
    Shared(McpClient),
    Owned(Box<McpConfig>),
}
pub struct McpToolsPlugin {
    id: String,
    ownership: Ownership,
}
impl McpToolsPlugin {
    /// Host retains ownership and must explicitly close the shared client.
    pub fn shared(id: impl Into<String>, client: McpClient) -> Self {
        Self {
            id: id.into(),
            ownership: Ownership::Shared(client),
        }
    }
    /// Each installation creates its own connection and owns its shutdown.
    pub fn owned(id: impl Into<String>, config: McpConfig) -> Self {
        Self {
            id: id.into(),
            ownership: Ownership::Owned(Box::new(config)),
        }
    }
}
#[async_trait]
impl Plugin for McpToolsPlugin {
    fn id(&self) -> &str {
        &self.id
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let client = match &self.ownership {
            Ownership::Shared(client) => client.clone(),
            Ownership::Owned(config) => {
                let client = McpClient::connect(config.as_ref().clone())
                    .await
                    .map_err(|e| PluginError::message(e.to_string()))?;
                let cancel = client.clone();
                context.on_cleanup(move || cancel.cancel());
                let shutdown = client.clone();
                context.on_shutdown(move || async move {
                    shutdown.close().await.map_err(|e| e.to_string())
                });
                client
            }
        };
        context.contribute(ToolProviderService(Arc::new(client)))
    }
}
