//! Typed method registration and native model/storage adapters for Rust sidecars.
use crate::*;
use async_trait::async_trait;
use eliteforge_agent_core::*;
use futures::future::BoxFuture;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};
type Method =
    dyn Fn(Value, BackendCallContext) -> BoxFuture<'static, Result<Value, RpcFault>> + Send + Sync;

#[derive(serde::Deserialize)]
pub struct BackendModelRequest<T> {
    pub model: String,
    pub request: T,
}

macro_rules! model_adapter {
    ($name:ident,$contract:ident,$method:ident,$input:ty) => {
        impl TypedBackend {
            pub fn $name(&mut self, id: &str, model: Arc<dyn $contract>) -> RuntimeResult<()> {
                self.method(
                    id,
                    stringify!($method),
                    move |input: BackendModelRequest<$input>, _| {
                        let model = model.clone();
                        async move {
                            model
                                .$method(&input.model, input.request)
                                .await
                                .map_err(|e| typed_fault(&e, e.to_string()))
                        }
                    },
                )
            }
        }
    };
}
model_adapter!(embedding_model, EmbeddingModel, embed, EmbeddingRequest);
model_adapter!(rerank_model, RerankModel, rerank, RerankRequest);
model_adapter!(
    content_detection_model,
    ContentDetectionModel,
    detect,
    ContentDetectionRequest
);
model_adapter!(
    image_model,
    ImageGenerationModel,
    generate,
    ImageGenerationRequest
);
model_adapter!(
    speech_recognition_model,
    SpeechRecognitionModel,
    transcribe,
    SpeechRecognitionRequest
);
model_adapter!(
    speech_synthesis_model,
    SpeechSynthesisModel,
    synthesize,
    SpeechSynthesisRequest
);
type ShutdownCallback = Box<dyn FnOnce() -> BoxFuture<'static, Result<(), RpcFault>> + Send>;
#[derive(Default)]
pub struct TypedBackend {
    methods: BTreeMap<(String, String), Arc<Method>>,
    pages: BTreeMap<(String, String), Arc<Method>>,
    shutdown: std::sync::Mutex<Vec<ShutdownCallback>>,
}
impl TypedBackend {
    pub fn on_shutdown<F, Fut>(&mut self, callback: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<(), RpcFault>> + Send + 'static,
    {
        self.shutdown
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .push(Box::new(|| Box::pin(callback())));
    }
    pub fn token_estimator(
        &mut self,
        id: &str,
        estimator: Arc<dyn TokenEstimator>,
    ) -> RuntimeResult<()> {
        for operation in ["message", "tools", "options", "request"] {
            let estimator = estimator.clone();
            self.method(id, operation, move |params: Value, _| {
                let estimator = estimator.clone();
                async move {
                    let model = decode(params["model"].clone())?;
                    let result = match operation {
                        "message" => {
                            estimator
                                .message(&model, &decode(params["message"].clone())?)
                                .await
                        }
                        "tools" => {
                            estimator
                                .tools(
                                    &model,
                                    &decode::<Vec<ToolDescriptor>>(params["tools"].clone())?,
                                )
                                .await
                        }
                        "options" => {
                            estimator
                                .options(&model, &decode(params["options"].clone())?)
                                .await
                        }
                        _ => {
                            estimator
                                .request(&model, &decode(params["request"].clone())?)
                                .await
                        }
                    };
                    result.map_err(|e| typed_fault(&e, e.to_string()))
                }
            })?;
        }
        Ok(())
    }
    pub fn model_discovery(
        &mut self,
        id: &str,
        discovery: Arc<dyn ModelDiscovery>,
    ) -> RuntimeResult<()> {
        self.method(id, "discover", move |_: Value, _| {
            let discovery = discovery.clone();
            async move {
                discovery
                    .discover()
                    .await
                    .map_err(|e| typed_fault(&e, e.to_string()))
            }
        })
    }
    pub fn long_term_memory(
        &mut self,
        id: &str,
        memory: Arc<dyn LongTermMemory>,
    ) -> RuntimeResult<()> {
        for operation in ["get", "search", "save", "delete"] {
            let memory = memory.clone();
            self.method(id, operation, move |params: Value, _| {
                let memory = memory.clone();
                async move {
                    let namespace = string(&params, "namespace")?;
                    let fault = |error: MemoryError| typed_fault(&error, error.to_string());
                    match operation {
                        "get" => encode(
                            memory
                                .get(namespace, string(&params, "id")?)
                                .await
                                .map_err(fault)?,
                        ),
                        "search" => encode(
                            memory
                                .search(namespace, &decode(params["query"].clone())?)
                                .await
                                .map_err(fault)?,
                        ),
                        "save" => encode(
                            memory
                                .save(
                                    namespace,
                                    &decode(params["value"].clone())?,
                                    params["expected"].as_str(),
                                )
                                .await
                                .map_err(fault)?,
                        ),
                        _ => {
                            memory
                                .delete(
                                    namespace,
                                    string(&params, "id")?,
                                    string(&params, "expected")?,
                                )
                                .await
                                .map_err(fault)?;
                            Ok(Value::Null)
                        }
                    }
                }
            })?;
        }
        Ok(())
    }
    pub fn video_model(
        &mut self,
        id: &str,
        model: Arc<dyn VideoGenerationModel>,
    ) -> RuntimeResult<()> {
        let issued = Arc::new(std::sync::Mutex::new(BTreeMap::<String, VideoJob>::new()));
        let submitting = model.clone();
        let submitted = issued.clone();
        self.method(
            id,
            "submit",
            move |input: BackendModelRequest<VideoGenerationRequest>, _| {
                let model = submitting.clone();
                let issued = submitted.clone();
                async move {
                    let job = model
                        .submit(&input.model, input.request)
                        .await
                        .map_err(|e| typed_fault(&e, e.to_string()))?;
                    if matches!(job.status, ModelJobStatus::Queued | ModelJobStatus::Running) {
                        issued
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(job.id.clone(), job.clone());
                    }
                    Ok(job)
                }
            },
        )?;
        for operation in ["query", "cancel"] {
            let model = model.clone();
            let issued = issued.clone();
            self.method(id, operation, move |params: Value, _| {
                let model = model.clone();
                let issued = issued.clone();
                async move {
                    let job: VideoJob = decode(params["job"].clone())?;
                    if operation == "query" {
                        let updated = model
                            .query(&job)
                            .await
                            .map_err(|e| typed_fault(&e, e.to_string()))?;
                        if !matches!(
                            updated.status,
                            ModelJobStatus::Queued | ModelJobStatus::Running
                        ) {
                            issued
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .remove(&job.id);
                        }
                        encode(updated)
                    } else {
                        model
                            .cancel(&job)
                            .await
                            .map_err(|e| typed_fault(&e, e.to_string()))?;
                        issued
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&job.id);
                        Ok(Value::Null)
                    }
                }
            })?;
        }
        self.on_shutdown(move || async move {
            let jobs = std::mem::take(&mut *issued.lock().unwrap_or_else(|e| e.into_inner()));
            let mut failures = Vec::new();
            for (_, job) in jobs {
                if let Err(error) = model.cancel(&job).await {
                    failures.push(error.to_string());
                }
            }
            if failures.is_empty() {
                Ok(())
            } else {
                Err(RpcFault::new(failures.join("; ")))
            }
        });
        Ok(())
    }
    pub fn method<T, R, F, Fut>(
        &mut self,
        implementation: &str,
        operation: &str,
        function: F,
    ) -> RuntimeResult<()>
    where
        T: DeserializeOwned + Send + 'static,
        R: Serialize + Send + 'static,
        F: Fn(T, BackendCallContext) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<R, RpcFault>> + Send + 'static,
    {
        let function = Arc::new(function);
        let method: Arc<Method> = Arc::new(move |params, context| {
            let function = function.clone();
            Box::pin(async move {
                let input =
                    serde_json::from_value(params).map_err(|e| RpcFault::new(e.to_string()))?;
                let output = function(input, context).await?;
                serde_json::to_value(output).map_err(|e| RpcFault::new(e.to_string()))
            })
        });
        let key = (implementation.into(), operation.into());
        if self.methods.contains_key(&key) {
            return Err(RuntimeError::Invalid("duplicate backend method".into()));
        }
        self.methods.insert(key, method);
        Ok(())
    }
    pub fn page_method<T, R, F, Fut>(
        &mut self,
        page: &str,
        method: &str,
        function: F,
    ) -> RuntimeResult<()>
    where
        T: DeserializeOwned + Send + 'static,
        R: Serialize + Send + 'static,
        F: Fn(T, BackendCallContext) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<R, RpcFault>> + Send + 'static,
    {
        if self.pages.contains_key(&(page.into(), method.into())) {
            return Err(RuntimeError::Invalid("duplicate page method".into()));
        }
        let temporary = format!("page.{page}");
        self.method(&temporary, method, function)?;
        let function = self
            .methods
            .remove(&(temporary, method.into()))
            .expect("registered page method");
        self.pages.insert((page.into(), method.into()), function);
        Ok(())
    }
    pub fn state_store(&mut self, id: &str, store: Arc<dyn StateStore>) -> RuntimeResult<()> {
        let leases = Arc::new(std::sync::Mutex::new(
            BTreeMap::<String, ExecutionLease>::new(),
        ));
        let closing = leases.clone();
        let closing_store = store.clone();
        self.on_shutdown(move || async move {
            let leases = std::mem::take(&mut *closing.lock().unwrap_or_else(|e| e.into_inner()));
            for lease in leases.into_values() {
                match closing_store.release(&lease).await {
                    Ok(()) | Err(StateError::LeaseLost) => {}
                    Err(error) => return Err(state_fault(error)),
                }
            }
            Ok(())
        });
        for operation in [
            "create",
            "load",
            "latest",
            "latest_scope",
            "claim",
            "renew",
            "commit",
            "release",
        ] {
            let store = store.clone();
            let leases = leases.clone();
            self.method(id, operation, move |params: Value, _| {
                let store = store.clone();
                let leases = leases.clone();
                async move {
                    match operation {
                        "create" => encode(
                            store
                                .create(
                                    decode(params["checkpoint"].clone())?,
                                    decode(params["previous_run"].clone())?,
                                )
                                .await
                                .map_err(state_fault)?,
                        ),
                        "load" => encode(
                            store
                                .load(decode(params["run_id"].clone())?)
                                .await
                                .map_err(state_fault)?,
                        ),
                        "latest" => encode(
                            store
                                .latest(string(&params, "session_id")?)
                                .await
                                .map_err(state_fault)?,
                        ),
                        "latest_scope" => encode(
                            store
                                .latest_scope(
                                    string(&params, "session_id")?,
                                    string(&params, "scope")?,
                                )
                                .await
                                .map_err(state_fault)?,
                        ),
                        "claim" => {
                            let lease = store
                                .claim(
                                    decode(params["run_id"].clone())?,
                                    decode(params["owner"].clone())?,
                                    decode(params["ttl"].clone())?,
                                )
                                .await
                                .map_err(state_fault)?;
                            leases.lock().unwrap_or_else(|e| e.into_inner()).insert(
                                format!("{}:{}:{}", lease.run_id, lease.owner, lease.generation),
                                lease.clone(),
                            );
                            encode(lease)
                        }
                        "renew" => encode(
                            store
                                .renew(
                                    &decode(params["lease"].clone())?,
                                    decode(params["ttl"].clone())?,
                                )
                                .await
                                .map_err(state_fault)?,
                        ),
                        "commit" => encode(
                            store
                                .commit(
                                    &decode(params["lease"].clone())?,
                                    decode(params["expected_revision"].clone())?,
                                    decode(params["next"].clone())?,
                                )
                                .await
                                .map_err(state_fault)?,
                        ),
                        _ => {
                            let lease: ExecutionLease = decode(params["lease"].clone())?;
                            store.release(&lease).await.map_err(state_fault)?;
                            leases
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .remove(&format!(
                                    "{}:{}:{}",
                                    lease.run_id, lease.owner, lease.generation
                                ));
                            Ok(Value::Null)
                        }
                    }
                }
            })?;
        }
        Ok(())
    }
    pub fn interaction_store(
        &mut self,
        id: &str,
        store: Arc<dyn InteractionStore>,
    ) -> RuntimeResult<()> {
        for operation in [
            "close_run",
            "publish",
            "publish_many",
            "get",
            "get_many",
            "find_many",
            "pending",
            "answer",
        ] {
            let store = store.clone();
            self.method(id, operation, move |params: Value, _| {
                let store = store.clone();
                async move {
                    match operation {
                        "close_run" => {
                            store
                                .close_run(
                                    string(&params, "session_id")?,
                                    decode(params["run_id"].clone())?,
                                )
                                .await
                                .map_err(state_fault)?;
                            Ok(Value::Null)
                        }
                        "publish" => {
                            store
                                .publish(&decode(params["record"].clone())?)
                                .await
                                .map_err(state_fault)?;
                            Ok(Value::Null)
                        }
                        "publish_many" => {
                            store
                                .publish_many(&decode::<Vec<InteractionRecord>>(
                                    params["records"].clone(),
                                )?)
                                .await
                                .map_err(state_fault)?;
                            Ok(Value::Null)
                        }
                        "get" => encode(
                            store
                                .get(
                                    string(&params, "session_id")?,
                                    decode(params["id"].clone())?,
                                )
                                .await
                                .map_err(state_fault)?,
                        ),
                        "get_many" => encode(
                            store
                                .get_many(
                                    string(&params, "session_id")?,
                                    &decode::<Vec<RunId>>(params["ids"].clone())?,
                                )
                                .await
                                .map_err(state_fault)?,
                        ),
                        "find_many" => encode(
                            store
                                .find_many(
                                    string(&params, "session_id")?,
                                    &decode::<Vec<RunId>>(params["ids"].clone())?,
                                )
                                .await
                                .map_err(state_fault)?,
                        ),
                        "pending" => encode(
                            store
                                .pending(string(&params, "session_id")?)
                                .await
                                .map_err(state_fault)?,
                        ),
                        _ => encode(
                            store
                                .answer(
                                    string(&params, "session_id")?,
                                    &decode(params["response"].clone())?,
                                )
                                .await
                                .map_err(|e| RpcFault::new(e.to_string()))?,
                        ),
                    }
                }
            })?;
        }
        Ok(())
    }
    pub fn conversation_store(
        &mut self,
        id: &str,
        store: Arc<dyn ConversationStore>,
    ) -> RuntimeResult<()> {
        self.method(id, "open", move |params: Value, context| {
            let store = store.clone();
            async move {
                let session = store
                    .open_branch(&decode(params["key"].clone())?)
                    .await
                    .map_err(conversation_fault)?;
                context.register_resource(Arc::new(ConversationResource(tokio::sync::Mutex::new(
                    session,
                ))))
            }
        })
    }
    pub fn chat_model(&mut self, id: &str, model: Arc<dyn ChatModel>) -> RuntimeResult<()> {
        self.method(id, "generate", move |params: Value, context| {
            let model = model.clone();
            async move {
                let mut request: ChatRequest = decode(params["request"].clone())?;
                request.extensions.insert(context.clone());
                let stream = BackendStream {
                    context,
                    failure: std::sync::Mutex::new(None),
                };
                let result = model
                    .generate(string(&params, "model")?, request, &stream)
                    .await
                    .map_err(|e| typed_fault(&e, e.to_string()))?;
                if let Some(error) = stream
                    .failure
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
                {
                    return Err(error);
                }
                Ok(result)
            }
        })
    }
    pub fn realtime_model(&mut self, id: &str, model: Arc<dyn RealtimeModel>) -> RuntimeResult<()> {
        self.method(id, "connect", move |params: Value, context| {
            let model = model.clone();
            async move {
                let cancellation = tokio_util::sync::CancellationToken::new();
                let session = model
                    .connect(
                        string(&params, "model")?,
                        decode(params["request"].clone())?,
                        cancellation.clone(),
                    )
                    .await
                    .map_err(|e| typed_fault(&e, e.to_string()))?;
                context.register_resource(Arc::new(RealtimeResource {
                    session,
                    cancellation,
                }))
            }
        })
    }
}
#[async_trait]
impl CapabilityBackend for TypedBackend {
    async fn shutdown(&self) -> Result<(), RpcFault> {
        let callbacks =
            std::mem::take(&mut *self.shutdown.lock().unwrap_or_else(|e| e.into_inner()));
        let mut errors = Vec::new();
        for callback in callbacks.into_iter().rev() {
            if let Err(error) = callback().await {
                errors.push(error.message);
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(RpcFault::new(errors.join("; ")))
        }
    }
    async fn initialize(
        &self,
        _: InstanceIdentity,
        _: Value,
        _: Vec<String>,
    ) -> Result<(), RpcFault> {
        Ok(())
    }
    async fn call(
        &self,
        implementation: &str,
        operation: &str,
        params: Value,
        context: BackendCallContext,
    ) -> Result<Value, RpcFault> {
        self.methods
            .get(&(implementation.into(), operation.into()))
            .ok_or_else(|| RpcFault::new("backend method not registered"))?(params, context)
        .await
    }
    async fn page(
        &self,
        page: &str,
        method: &str,
        params: Value,
        context: BackendCallContext,
    ) -> Result<Value, RpcFault> {
        self.pages
            .get(&(page.into(), method.into()))
            .ok_or_else(|| RpcFault::new("page method not registered"))?(params, context)
        .await
    }
}
fn string<'a>(params: &'a Value, field: &str) -> Result<&'a str, RpcFault> {
    params[field]
        .as_str()
        .ok_or_else(|| RpcFault::new(format!("missing {field}")))
}
fn decode<T: DeserializeOwned>(params: Value) -> Result<T, RpcFault> {
    serde_json::from_value(params).map_err(|e| RpcFault::new(e.to_string()))
}
fn encode<T: Serialize>(value: T) -> Result<Value, RpcFault> {
    serde_json::to_value(value).map_err(|e| RpcFault::new(e.to_string()))
}
fn state_fault(error: StateError) -> RpcFault {
    typed_fault(&error, error.to_string())
}
fn conversation_fault(error: ConversationError) -> RpcFault {
    typed_fault(&error, error.to_string())
}
struct ConversationResource(tokio::sync::Mutex<Box<dyn ConversationSession>>);
#[async_trait]
impl BackendResource for ConversationResource {
    async fn call(
        &self,
        operation: &str,
        params: Value,
        _: BackendCallContext,
    ) -> Result<Value, RpcFault> {
        let mut session = self.0.lock().await;
        match operation {
            "snapshot" => encode(session.snapshot().await.map_err(conversation_fault)?),
            "append" => {
                session
                    .append(&decode::<Vec<Message>>(params["messages"].clone())?)
                    .await
                    .map_err(conversation_fault)?;
                Ok(Value::Null)
            }
            "append_at" => {
                session
                    .append_at(
                        decode(params["revision"].clone())?,
                        &decode::<Vec<Message>>(params["messages"].clone())?,
                    )
                    .await
                    .map_err(conversation_fault)?;
                Ok(Value::Null)
            }
            "finish" => {
                session
                    .finish(decode(params["outcome"].clone())?)
                    .await
                    .map_err(conversation_fault)?;
                Ok(Value::Null)
            }
            _ => Err(RpcFault::new("unknown conversation resource method")),
        }
    }
    async fn close(&self) -> Result<(), RpcFault> {
        self.0
            .lock()
            .await
            .finish(ConversationOutcome::Stopped)
            .await
            .map_err(conversation_fault)
    }
}
struct BackendStream {
    context: BackendCallContext,
    failure: std::sync::Mutex<Option<RpcFault>>,
}
#[async_trait]
impl ChatStream for BackendStream {
    async fn text_delta(&self, text: &str) {
        if let Err(error) = self
            .context
            .host_call("host.stream.text", json!({"text":text}))
            .await
        {
            *self.failure.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
        }
    }
    async fn reasoning_delta(&self, text: &str) {
        if let Err(error) = self
            .context
            .host_call("host.stream.reasoning", json!({"text":text}))
            .await
        {
            *self.failure.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
        }
    }
}
struct RealtimeResource {
    session: Arc<dyn RealtimeSession>,
    cancellation: tokio_util::sync::CancellationToken,
}
impl Drop for RealtimeResource {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}
#[async_trait]
impl BackendResource for RealtimeResource {
    async fn call(
        &self,
        operation: &str,
        params: Value,
        _: BackendCallContext,
    ) -> Result<Value, RpcFault> {
        match operation {
            "send" => {
                self.session
                    .send(decode(params["input"].clone())?)
                    .await
                    .map_err(|e| typed_fault(&e, e.to_string()))?;
                Ok(Value::Null)
            }
            "next" => encode(
                self.session
                    .next()
                    .await
                    .map_err(|e| typed_fault(&e, e.to_string()))?,
            ),
            _ => Err(RpcFault::new("unknown realtime resource method")),
        }
    }
    async fn close(&self) -> Result<(), RpcFault> {
        self.cancellation.cancel();
        self.session
            .close()
            .await
            .map_err(|e| typed_fault(&e, e.to_string()))
    }
}
