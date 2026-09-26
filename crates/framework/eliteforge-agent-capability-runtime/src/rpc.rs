use crate::*;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{Notify, Semaphore, mpsc, oneshot},
};

pub const MAX_JSON_FRAME: usize = 8 * 1024 * 1024;
pub const MAX_BINARY_FRAME: usize = 64 * 1024 * 1024;
pub const BINARY_CHUNK_BYTES: usize = 64 * 1024;
pub const MAX_RPC_PAYLOAD: usize = 64 * 1024 * 1024;
fn payload_frames(value: Value) -> RuntimeResult<VecDeque<WireFrame>> {
    let bytes = serde_json::to_vec(&value)?;
    if bytes.len() <= MAX_JSON_FRAME {
        return Ok(VecDeque::from([WireFrame::Json(value)]));
    }
    if bytes.len() > MAX_RPC_PAYLOAD {
        return Err(RuntimeError::Invalid(
            "RPC payload exceeds transfer limit".into(),
        ));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let mut frames = VecDeque::from([WireFrame::Json(
        json!({"jsonrpc":"2.0","method":"$/payload","params":{"id":id,"size":bytes.len(),"message":value["id"]}}),
    )]);
    for (index, chunk) in bytes.chunks(BINARY_CHUNK_BYTES).enumerate() {
        frames.push_back(WireFrame::Binary {
            channel: format!("rpc:{id}:{index}"),
            bytes: chunk.to_vec(),
        });
    }
    Ok(frames)
}
struct PayloadAssembly {
    size: usize,
    sequence: usize,
    message: Option<String>,
    bytes: Vec<u8>,
}
#[derive(Debug, Clone)]
pub enum WireFrame {
    Json(Value),
    Binary { channel: String, bytes: Vec<u8> },
}
pub async fn read_frame(reader: &mut (impl AsyncRead + Unpin)) -> RuntimeResult<Option<WireFrame>> {
    let kind = match reader.read_u8().await {
        Ok(kind) => kind,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let size = reader.read_u32().await? as usize;
    let limit = match kind {
        0 => MAX_JSON_FRAME,
        1 => MAX_BINARY_FRAME,
        _ => return Err(RuntimeError::Invalid("unknown frame kind".into())),
    };
    if size > limit {
        return Err(RuntimeError::Invalid("RPC frame exceeds size limit".into()));
    }
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes).await?;
    if kind == 0 {
        return Ok(Some(WireFrame::Json(serde_json::from_slice(&bytes)?)));
    }
    if size < 2 {
        return Err(RuntimeError::Invalid("truncated binary frame".into()));
    }
    let length = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
    if length + 2 > size {
        return Err(RuntimeError::Invalid(
            "invalid binary channel length".into(),
        ));
    }
    let channel = String::from_utf8(bytes[2..length + 2].to_vec())
        .map_err(|e| RuntimeError::Invalid(e.to_string()))?;
    Ok(Some(WireFrame::Binary {
        channel,
        bytes: bytes[length + 2..].to_vec(),
    }))
}
pub async fn write_frame(
    writer: &mut (impl AsyncWrite + Unpin),
    frame: &WireFrame,
) -> RuntimeResult<()> {
    let (kind, bytes, limit) = match frame {
        WireFrame::Json(value) => (0, serde_json::to_vec(value)?, MAX_JSON_FRAME),
        WireFrame::Binary { channel, bytes } => {
            let length = u16::try_from(channel.len())
                .map_err(|_| RuntimeError::Invalid("binary channel too long".into()))?;
            let mut payload = Vec::with_capacity(2 + channel.len() + bytes.len());
            payload.extend_from_slice(&length.to_be_bytes());
            payload.extend_from_slice(channel.as_bytes());
            payload.extend_from_slice(bytes);
            (1, payload, MAX_BINARY_FRAME)
        }
    };
    if bytes.len() > limit {
        return Err(RuntimeError::Invalid("RPC frame exceeds size limit".into()));
    }
    writer.write_u8(kind).await?;
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
    #[serde(default)]
    pub scope: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcFault {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}
impl RpcFault {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            code: -32000,
            message: message.into(),
            data: None,
        }
    }
}
pub const INDETERMINATE_FAULT_CODE: i64 = -32098;
impl From<RuntimeError> for RpcFault {
    fn from(error: RuntimeError) -> Self {
        match error {
            RuntimeError::Fault(fault) => fault,
            RuntimeError::Indeterminate(message) => Self {
                code: INDETERMINATE_FAULT_CODE,
                message,
                data: None,
            },
            error => Self::new(error.to_string()),
        }
    }
}
#[async_trait]
pub trait RpcHandler: Send + Sync {
    async fn handle(&self, request: RpcRequest, peer: RpcPeer) -> Result<Value, RpcFault>;
}
pub struct RejectRequests;
#[async_trait]
impl RpcHandler for RejectRequests {
    async fn handle(&self, request: RpcRequest, _: RpcPeer) -> Result<Value, RpcFault> {
        Err(RpcFault::new(format!(
            "unsupported RPC method: {}",
            request.method
        )))
    }
}

type Reply = oneshot::Sender<RuntimeResult<Value>>;
struct PeerState {
    prefix: String,
    next: AtomicU64,
    writer: mpsc::Sender<WireFrame>,
    control: mpsc::Sender<WireFrame>,
    binary_writer: mpsc::Sender<WireFrame>,
    pending: Mutex<HashMap<String, Reply>>,
    scopes: Mutex<HashMap<String, mpsc::Sender<IncomingCall>>>,
    tasks: Mutex<HashMap<String, tokio::task::AbortHandle>>,
    callbacks: Mutex<HashMap<String, tokio_util::sync::CancellationToken>>,
    closed: AtomicBool,
    stopped: Notify,
    cancellation: tokio_util::sync::CancellationToken,
    binary: tokio::sync::broadcast::Sender<(String, Vec<u8>)>,
}
#[derive(Clone)]
pub struct RpcPeer {
    state: Arc<PeerState>,
}
pub struct IncomingCall {
    pub request: RpcRequest,
    writer: mpsc::Sender<WireFrame>,
    answered: bool,
    cancellation: tokio_util::sync::CancellationToken,
    state: Weak<PeerState>,
}
impl IncomingCall {
    pub async fn cancelled(&self) {
        self.cancellation.cancelled().await;
    }
    pub async fn respond(mut self, result: Result<Value, RpcFault>) -> RuntimeResult<()> {
        self.answered = true;
        self.writer
            .send(response_frame(&self.request.id, result))
            .await
            .map_err(|_| RuntimeError::Rpc("peer closed".into()))
    }
}
impl Drop for IncomingCall {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade() {
            state
                .callbacks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&self.request.id);
        }
        if !self.answered {
            let _ = self.writer.try_send(response_frame(
                &self.request.id,
                Err(RpcFault::new("callback scope closed")),
            ));
        }
    }
}
fn response_frame(id: &str, result: Result<Value, RpcFault>) -> WireFrame {
    WireFrame::Json(match result {
        Ok(value) => json!({"jsonrpc":"2.0","id":id,"result":value}),
        Err(error) => json!({"jsonrpc":"2.0","id":id,"error":error}),
    })
}
pub struct RpcScope {
    id: String,
    state: Weak<PeerState>,
    pub incoming: mpsc::Receiver<IncomingCall>,
}
impl RpcScope {
    pub fn id(&self) -> &str {
        &self.id
    }
}
impl Drop for RpcScope {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade() {
            state
                .scopes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&self.id);
        }
    }
}
struct PendingGuard {
    id: String,
    state: Weak<PeerState>,
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade()
            && state
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&self.id)
                .is_some()
            && state
                .control
                .try_send(WireFrame::Json(
                    json!({"jsonrpc":"2.0","method":"$/cancel","params":{"id":self.id}}),
                ))
                .is_err()
        {
            close_state(&state);
        }
    }
}
impl RpcPeer {
    pub fn new(
        reader: impl AsyncRead + Unpin + Send + 'static,
        writer: impl AsyncWrite + Unpin + Send + 'static,
        prefix: impl Into<String>,
        handler: Arc<dyn RpcHandler>,
    ) -> Self {
        let (sender, mut receiver) = mpsc::channel(128);
        let (control, mut control_receiver) = mpsc::channel(128);
        let (binary_writer, mut binary_receiver) = mpsc::channel(4);
        let (binary, _) = tokio::sync::broadcast::channel(32);
        let state = Arc::new(PeerState {
            prefix: prefix.into(),
            next: AtomicU64::new(1),
            writer: sender,
            control,
            binary_writer,
            pending: Mutex::new(HashMap::new()),
            scopes: Mutex::new(HashMap::new()),
            tasks: Mutex::new(HashMap::new()),
            callbacks: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
            stopped: Notify::new(),
            cancellation: tokio_util::sync::CancellationToken::new(),
            binary,
        });
        let weak = Arc::downgrade(&state);
        let cancelled = state.cancellation.clone();
        tokio::spawn(async move {
            let mut writer = writer;
            let mut chunks = VecDeque::new();
            loop {
                let frame = if chunks.is_empty() {
                    tokio::select! {biased;_=cancelled.cancelled()=>None,frame=control_receiver.recv()=>frame,frame=receiver.recv()=>frame,frame=binary_receiver.recv()=>frame}
                } else {
                    tokio::select! {biased;_=cancelled.cancelled()=>None,frame=control_receiver.recv()=>frame,frame=async{chunks.pop_front()}=>frame}
                };
                let Some(frame) = frame else {
                    break;
                };
                let frame = if let WireFrame::Json(value) = frame {
                    let mut frames = match payload_frames(value) {
                        Ok(frames) => frames,
                        Err(_) => {
                            if let Some(state) = weak.upgrade() {
                                close_state(&state);
                            }
                            break;
                        }
                    };
                    let first = frames.pop_front().expect("payload has a header");
                    frames.append(&mut chunks);
                    chunks = frames;
                    first
                } else {
                    frame
                };
                let result = tokio::select! {biased;_ = cancelled.cancelled()=>break,result=write_frame(&mut writer,&frame)=>result};
                if result.is_err() {
                    if let Some(state) = weak.upgrade() {
                        close_state(&state);
                    }
                    break;
                }
            }
            let _ = writer.shutdown().await;
        });
        let weak = Arc::downgrade(&state);
        let cancelled = state.cancellation.clone();
        tokio::spawn(async move {
            let mut reader = reader;
            let mut payloads = HashMap::<String, PayloadAssembly>::new();
            let mut cancelled_payloads = HashMap::<String, usize>::new();
            let mut cancelled_requests = HashSet::<String>::new();
            let mut cancellation_order = VecDeque::<String>::new();
            let capacity = Arc::new(Semaphore::new(256));
            loop {
                let frame = tokio::select! { _ = cancelled.cancelled() => break, frame = read_frame(&mut reader) => frame };
                let Some(state) = weak.upgrade() else {
                    break;
                };
                let mut frame = match frame {
                    Ok(Some(frame)) => frame,
                    _ => {
                        close_state(&state);
                        break;
                    }
                };
                if state.closed.load(Ordering::Acquire) {
                    break;
                }
                if let WireFrame::Json(value) = &frame {
                    if value["method"] == "$/payload" {
                        let id = value["params"]["id"].as_str().unwrap_or_default();
                        let size = value["params"]["size"].as_u64().unwrap_or(0) as usize;
                        if uuid::Uuid::parse_str(id).is_err()
                            || size <= MAX_JSON_FRAME
                            || size > MAX_RPC_PAYLOAD
                            || payloads.len() >= 4
                            || payloads.contains_key(id)
                        {
                            close_state(&state);
                            break;
                        }
                        if value["params"]["message"]
                            .as_str()
                            .is_some_and(|message| cancelled_requests.contains(message))
                        {
                            if cancelled_payloads.len() >= 1024 {
                                close_state(&state);
                                break;
                            }
                            cancelled_payloads
                                .insert(id.into(), size.div_ceil(BINARY_CHUNK_BYTES) - 1);
                            continue;
                        }
                        payloads.insert(
                            id.into(),
                            PayloadAssembly {
                                size,
                                sequence: 0,
                                message: value["params"]["message"].as_str().map(str::to_owned),
                                bytes: Vec::new(),
                            },
                        );
                        continue;
                    }
                    if value["method"] == "$/cancel"
                        && let Some(message) = value["params"]["id"].as_str()
                    {
                        if cancelled_requests.insert(message.into()) {
                            cancellation_order.push_back(message.into());
                        }
                        while cancellation_order.len() > 1024 {
                            if let Some(id) = cancellation_order.pop_front() {
                                cancelled_requests.remove(&id);
                            }
                        }
                        let ids: Vec<_> = payloads
                            .iter()
                            .filter(|(_, payload)| payload.message.as_deref() == Some(message))
                            .map(|(id, _)| id.clone())
                            .collect();
                        for id in ids {
                            let payload = payloads.remove(&id).expect("selected payload");
                            if cancelled_payloads.len() < 1024 {
                                cancelled_payloads
                                    .insert(id, payload.size.div_ceil(BINARY_CHUNK_BYTES) - 1);
                            }
                        }
                    }
                }
                if let WireFrame::Binary { channel, bytes } = &frame
                    && let Some(channel) = channel.strip_prefix("rpc:")
                {
                    let Some((id, index)) = channel.rsplit_once(':') else {
                        close_state(&state);
                        break;
                    };
                    if let Some(last) = cancelled_payloads.get(id).copied() {
                        if index.parse::<usize>().ok() == Some(last) {
                            cancelled_payloads.remove(id);
                        }
                        continue;
                    }
                    let Some(payload) = payloads.get_mut(id) else {
                        close_state(&state);
                        break;
                    };
                    if index.parse::<usize>().ok() != Some(payload.sequence)
                        || bytes.len() > BINARY_CHUNK_BYTES
                        || bytes.is_empty()
                        || payload.bytes.len().saturating_add(bytes.len()) > payload.size
                    {
                        close_state(&state);
                        break;
                    }
                    payload.sequence += 1;
                    payload.bytes.extend_from_slice(bytes);
                    if payload.bytes.len() != payload.size {
                        continue;
                    }
                    let payload = payloads.remove(id).expect("validated payload");
                    frame = match serde_json::from_slice(&payload.bytes) {
                        Ok(value) => WireFrame::Json(value),
                        Err(_) => {
                            close_state(&state);
                            break;
                        }
                    };
                }
                match frame {
                    WireFrame::Binary { channel, bytes } => {
                        let _ = state.binary.send((channel, bytes));
                    }
                    WireFrame::Json(value) => {
                        if value.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
                            close_state(&state);
                            break;
                        }
                        if let Some(method) = value.get("method").and_then(Value::as_str) {
                            if method == "$/cancel" {
                                if let Some(id) =
                                    value.pointer("/params/id").and_then(Value::as_str)
                                    && let Some(token) = state
                                        .callbacks
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .get(id)
                                {
                                    token.cancel();
                                }
                                if let Some(id) =
                                    value.pointer("/params/id").and_then(Value::as_str)
                                    && let Some(task) = state
                                        .tasks
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .remove(id)
                                {
                                    task.abort();
                                }
                                continue;
                            }
                            let request: RpcRequest = match serde_json::from_value(value) {
                                Ok(request) => request,
                                Err(_) => {
                                    close_state(&state);
                                    break;
                                }
                            };
                            if cancelled_requests.contains(&request.id) {
                                let _ = state.control.try_send(response_frame(
                                    &request.id,
                                    Err(RpcFault {
                                        code: -32800,
                                        message: "request cancelled before dispatch".into(),
                                        data: None,
                                    }),
                                ));
                                continue;
                            }
                            let route = request.scope.as_ref().and_then(|scope| {
                                state
                                    .scopes
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .get(scope)
                                    .cloned()
                            });
                            if let Some(route) = route {
                                let cancellation = tokio_util::sync::CancellationToken::new();
                                state
                                    .callbacks
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .insert(request.id.clone(), cancellation.clone());
                                let incoming = IncomingCall {
                                    request,
                                    writer: state.control.clone(),
                                    answered: false,
                                    cancellation,
                                    state: Arc::downgrade(&state),
                                };
                                if let Err(error) = route.try_send(incoming) {
                                    drop(error.into_inner());
                                }
                            } else {
                                if request.method.starts_with("host.") {
                                    let _ = state.control.try_send(response_frame(
                                        &request.id,
                                        Err(RpcFault::new("invalid or closed host scope")),
                                    ));
                                    continue;
                                }
                                let permit = match capacity.clone().try_acquire_owned() {
                                    Ok(permit) => permit,
                                    Err(_) => {
                                        let _ = state.control.try_send(response_frame(
                                            &request.id,
                                            Err(RpcFault::new("too many requests")),
                                        ));
                                        continue;
                                    }
                                };
                                let id = request.id.clone();
                                let handler = handler.clone();
                                let peer = RpcPeer {
                                    state: state.clone(),
                                };
                                let request_id = id.clone();
                                let (start, started) = oneshot::channel();
                                let task = tokio::spawn(async move {
                                    let _ = started.await;
                                    let _permit = permit;
                                    use futures::FutureExt;
                                    let result = std::panic::AssertUnwindSafe(
                                        handler.handle(request, peer.clone()),
                                    )
                                    .catch_unwind()
                                    .await
                                    .unwrap_or_else(|_| {
                                        Err(RpcFault::from(RuntimeError::Indeterminate(
                                            "backend handler panicked; effects may have completed"
                                                .into(),
                                        )))
                                    });
                                    let _ = peer
                                        .state
                                        .control
                                        .send(response_frame(&request_id, result))
                                        .await;
                                    peer.state
                                        .tasks
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .remove(&request_id);
                                });
                                state
                                    .tasks
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .insert(id, task.abort_handle());
                                let _ = start.send(());
                            }
                        } else if let Some(id) = value.get("id").and_then(Value::as_str) {
                            let pending = state
                                .pending
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .remove(id);
                            if let Some(pending) = pending {
                                let result = if let Some(error) = value.get("error") {
                                    serde_json::from_value(error.clone())
                                        .map_err(|_| {
                                            RuntimeError::Indeterminate(
                                                "malformed RPC error".into(),
                                            )
                                        })
                                        .and_then(|error: RpcFault| {
                                            if error.code == INDETERMINATE_FAULT_CODE {
                                                Err(RuntimeError::Indeterminate(error.message))
                                            } else {
                                                Err(RuntimeError::Fault(error))
                                            }
                                        })
                                } else {
                                    value.get("result").cloned().ok_or_else(|| {
                                        RuntimeError::Indeterminate("missing RPC result".into())
                                    })
                                };
                                let _ = pending.send(result);
                            }
                        } else {
                            close_state(&state);
                            break;
                        }
                    }
                }
            }
        });
        Self { state }
    }
    pub fn scope(&self) -> RpcScope {
        let id = uuid::Uuid::new_v4().to_string();
        let (sender, incoming) = mpsc::channel(128);
        self.state
            .scopes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.clone(), sender);
        RpcScope {
            id,
            state: Arc::downgrade(&self.state),
            incoming,
        }
    }
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> RuntimeResult<Value> {
        self.request_in_scope(None, method, params, timeout).await
    }
    pub async fn request_in_scope(
        &self,
        scope: Option<&str>,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> RuntimeResult<Value> {
        if self.is_closed() {
            return Err(RuntimeError::Rpc("peer closed".into()));
        }
        let id = format!(
            "{}:{}",
            self.state.prefix,
            self.state.next.fetch_add(1, Ordering::Relaxed)
        );
        let (sender, receiver) = oneshot::channel();
        {
            let mut pending = self.state.pending.lock().unwrap_or_else(|e| e.into_inner());
            if pending.len() >= 256 {
                return Err(RuntimeError::Rpc("too many pending calls".into()));
            }
            pending.insert(id.clone(), sender);
        }
        let _guard = PendingGuard {
            id: id.clone(),
            state: Arc::downgrade(&self.state),
        };
        let frame = WireFrame::Json(
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params,"scope":scope}),
        );
        if let WireFrame::Json(value) = &frame
            && serde_json::to_vec(value)?.len() > MAX_RPC_PAYLOAD
        {
            return Err(RuntimeError::Invalid(
                "RPC payload exceeds transfer limit".into(),
            ));
        }
        tokio::time::timeout(timeout, async {
            self.state
                .writer
                .send(frame)
                .await
                .map_err(|_| RuntimeError::Rpc("peer closed".into()))?;
            receiver.await.map_err(|_| {
                RuntimeError::Indeterminate("connection lost after request admission".into())
            })?
        })
        .await
        .map_err(|_| RuntimeError::Indeterminate(format!("RPC timed out: {method}")))?
    }
    pub async fn send_binary(&self, channel: String, bytes: Vec<u8>) -> RuntimeResult<()> {
        if channel.starts_with("rpc:")
            || channel.len() > u16::MAX as usize
            || bytes.len().saturating_add(channel.len() + 2) > MAX_BINARY_FRAME
        {
            return Err(RuntimeError::Invalid("binary payload too large".into()));
        }
        self.state
            .binary_writer
            .send(WireFrame::Binary { channel, bytes })
            .await
            .map_err(|_| RuntimeError::Rpc("peer closed".into()))
    }
    pub fn subscribe_binary(&self) -> tokio::sync::broadcast::Receiver<(String, Vec<u8>)> {
        self.state.binary.subscribe()
    }
    pub fn is_closed(&self) -> bool {
        self.state.closed.load(Ordering::Acquire)
    }
    pub fn close(&self) {
        close_state(&self.state);
    }
    pub async fn closed(&self) {
        loop {
            let notified = self.state.stopped.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_closed() {
                return;
            }
            notified.await;
        }
    }
}
fn close_state(state: &PeerState) {
    if state.closed.swap(true, Ordering::AcqRel) {
        return;
    }
    let pending = std::mem::take(&mut *state.pending.lock().unwrap_or_else(|e| e.into_inner()));
    for (_, sender) in pending {
        let _ = sender.send(Err(RuntimeError::Indeterminate(
            "connection lost; operation may have completed".into(),
        )));
    }
    state
        .scopes
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    let tasks = std::mem::take(&mut *state.tasks.lock().unwrap_or_else(|e| e.into_inner()));
    for (_, task) in tasks {
        task.abort();
    }
    for (_, token) in
        std::mem::take(&mut *state.callbacks.lock().unwrap_or_else(|e| e.into_inner()))
    {
        token.cancel();
    }
    state.cancellation.cancel();
    state.stopped.notify_waiters();
}
/// Backend entry point. The same transport is used by packaged and development hosts.
pub async fn serve_backend(handler: Arc<dyn RpcHandler>) -> RuntimeResult<()> {
    let peer = RpcPeer::new(tokio::io::stdin(), tokio::io::stdout(), "backend", handler);
    peer.closed().await;
    Ok(())
}

impl Drop for PeerState {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}
