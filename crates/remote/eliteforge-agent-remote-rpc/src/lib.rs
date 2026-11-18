//! Product-neutral RPC contract shared by the remote server and product packs.

use async_trait::async_trait;
use eliteforge_agent_remote_protocol::TransportMode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

pub const SESSION_LIST: &str = "session.list";
pub const SESSION_HISTORY: &str = "session.history";
pub const SESSION_CREATE: &str = "session.create";
pub const SESSION_SEND_MESSAGE: &str = "session.sendMessage";
pub const SESSION_STOP_RUN: &str = "session.stopRun";
pub const SESSION_WATCH_EVENTS: &str = "session.watchEvents";
pub const SESSION_UNWATCH_EVENTS: &str = "session.unwatchEvents";

#[derive(Clone, Debug)]
pub struct RemotePrincipal {
    pub device_id: String,
    pub scopes: Vec<String>,
    pub profile: Option<ProfileSummary>,
    pub transport: TransportMode,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileSummary {
    pub username: String,
    pub display_name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcError {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub details: Option<Value>,
}

impl RpcError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details: None,
        }
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new("RPC_INVALID_PARAMS", message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new("RPC_FORBIDDEN", message)
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new("RPC_UNSUPPORTED", message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new("RPC_INTERNAL", message)
    }
}

/// RPC output side for streaming/subscription methods. It is intentionally
/// called `EventEmitter`, not `EventSink`, to avoid confusion with the agent
/// framework's internal EventSink.
#[derive(Clone)]
pub struct EventEmitter {
    tx: UnboundedSender<Value>,
    cancel: CancellationToken,
}

impl EventEmitter {
    pub fn new(tx: UnboundedSender<Value>, cancel: CancellationToken) -> Self {
        Self { tx, cancel }
    }

    pub fn emit(&self, data: Value) -> Result<(), RpcError> {
        self.tx
            .send(data)
            .map_err(|_| RpcError::internal("remote client disconnected"))
    }

    pub fn cancelled(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionListParams {
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub filter: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub id: String,
    pub product_code: String,
    pub title: String,
    pub updated_at: Value,
    pub status: String,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionListResult {
    pub sessions: Vec<SessionSummary>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHistoryParams {
    pub session_id: String,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
    /// Product-specific context (for example a Dude code) the client already
    /// knows; the pack may forward it to the product's own API.
    #[serde(default)]
    pub metadata: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: Value,
    pub role: String,
    pub content: String,
    pub created_at: Value,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHistoryResult {
    pub messages: Vec<Message>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCreateParams {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSendMessageParams {
    pub session_id: String,
    pub content: String,
    #[serde(default)]
    pub attachments: Option<Value>,
    #[serde(default)]
    pub client_capabilities: Option<Value>,
    /// Product-owned model selection (opaque to the framework).
    ///
    /// The framework never interprets it; a pack forwards it to its product so
    /// a remote client can pick the same model as the desktop client instead of
    /// silently falling back to a placeholder route.
    #[serde(default)]
    pub model_selection: Option<Value>,
    /// Reasoning effort name (`Low`, `High`, ...); product-owned vocabulary.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageResult {
    pub run_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStopRunParams {
    pub session_id: String,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StopRunResult {
    pub cancelled: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionWatchEventsParams {
    pub session_id: String,
    #[serde(default)]
    pub cursor: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Subscription {
    pub subscription_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUnwatchEventsParams {
    pub subscription_id: String,
}

#[async_trait]
pub trait SessionRpc: Send + Sync {
    async fn list(&self, ctx: &RemotePrincipal, params: SessionListParams) -> Result<SessionListResult, RpcError>;
    async fn history(&self, ctx: &RemotePrincipal, params: SessionHistoryParams) -> Result<SessionHistoryResult, RpcError>;
    async fn create(&self, ctx: &RemotePrincipal, params: SessionCreateParams) -> Result<SessionSummary, RpcError>;
    async fn send_message(
        &self,
        ctx: &RemotePrincipal,
        params: SessionSendMessageParams,
        events: EventEmitter,
    ) -> Result<SendMessageResult, RpcError>;
    async fn stop_run(&self, ctx: &RemotePrincipal, params: SessionStopRunParams) -> Result<StopRunResult, RpcError>;
    async fn watch_events(
        &self,
        ctx: &RemotePrincipal,
        params: SessionWatchEventsParams,
        events: EventEmitter,
    ) -> Result<Subscription, RpcError>;
    async fn unwatch_events(&self, ctx: &RemotePrincipal, params: SessionUnwatchEventsParams) -> Result<(), RpcError>;
}

#[async_trait]
pub trait RpcMethod: Send + Sync {
    async fn call(&self, ctx: &RemotePrincipal, params: Value) -> Result<Value, RpcError>;
}

#[derive(Clone, Default)]
pub struct RpcRegistry {
    methods: HashMap<String, Arc<dyn RpcMethod>>,
    session: Option<Arc<dyn SessionRpc>>,
}

impl RpcRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_session_rpc(&mut self, session: Arc<dyn SessionRpc>) {
        self.session = Some(session);
    }

    pub fn session_rpc(&self) -> Option<Arc<dyn SessionRpc>> {
        self.session.clone()
    }

    pub fn register(&mut self, name: impl Into<String>, method: Arc<dyn RpcMethod>) {
        self.methods.insert(name.into(), method);
    }

    pub fn contains(&self, name: &str) -> bool {
        self.session.is_some() && name.starts_with("session.") || self.methods.contains_key(name)
    }

    pub fn method_names(&self) -> Vec<String> {
        let mut names = self.methods.keys().cloned().collect::<Vec<_>>();
        if self.session.is_some() {
            names.extend(
                [
                    SESSION_LIST,
                    SESSION_HISTORY,
                    SESSION_CREATE,
                    SESSION_SEND_MESSAGE,
                    SESSION_STOP_RUN,
                    SESSION_WATCH_EVENTS,
                    SESSION_UNWATCH_EVENTS,
                ]
                .into_iter()
                .map(str::to_owned),
            );
        }
        names.sort();
        names.dedup();
        names
    }

    pub async fn dispatch(&self, ctx: &RemotePrincipal, method: &str, params: Value) -> Result<Value, RpcError> {
        let handler = self
            .methods
            .get(method)
            .cloned()
            .ok_or_else(|| RpcError::new("RPC_METHOD_NOT_FOUND", format!("unknown method: {method}")))?;
        handler.call(ctx, params).await
    }
}

/// A product supplies its business RPC implementation and product-specific
/// methods. The framework never imports product crates.
pub trait RemotePack: Send + Sync {
    fn session_rpc(&self) -> Option<Arc<dyn SessionRpc>> {
        None
    }

    fn register(&self, _registry: &mut RpcRegistry) {}
}
