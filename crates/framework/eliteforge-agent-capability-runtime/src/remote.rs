use crate::*;
use async_trait::async_trait;
use eliteforge_agent_core::*;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

#[async_trait]
pub trait ScopedHost: Send {
    fn trace_context(&self) -> Option<TraceContext> {
        None
    }
    async fn call(&mut self, request: RpcRequest) -> Result<Value, RpcFault>;
}
pub struct NoScopedHost;
#[async_trait]
impl ScopedHost for NoScopedHost {
    async fn call(&mut self, _: RpcRequest) -> Result<Value, RpcFault> {
        Err(RpcFault::new("host access is not granted"))
    }
}
#[derive(Clone)]
pub struct RemoteImplementation {
    pub instance: Arc<BackendInstance>,
    pub declaration: ExtensionImplementation,
}
impl RemoteImplementation {
    pub async fn invoke(&self, operation: &str, params: Value) -> RuntimeResult<Value> {
        self.invoke_scoped(operation, params, &mut NoScopedHost)
            .await
    }
    pub async fn invoke_scoped(
        &self,
        operation: &str,
        params: Value,
        host: &mut dyn ScopedHost,
    ) -> RuntimeResult<Value> {
        let _lease = self.instance.occupy(
            "extension_call",
            format!("{}.{operation}", self.declaration.id),
        )?;
        let peer = self.instance.peer().await?;
        let mut scope = peer.scope();
        let scope_id = scope.id().to_owned();
        let request = peer.request_in_scope(
            Some(&scope_id),
            "extension.call",
            json!({"implementation":self.declaration.id,"operation":operation,"params":params,"trace_context":host.trace_context()}),
            Duration::from_secs(60),
        );
        tokio::pin!(request);
        loop {
            tokio::select! {
                biased;
                incoming = scope.incoming.recv() => { let Some(incoming) = incoming else { return request.await; }; let result = tokio::select! { biased; _ = incoming.cancelled() => Err(RpcFault::new("host callback cancelled")), result = host.call(incoming.request.clone()) => result, result = &mut request => return result }; incoming.respond(result).await?; },
                result = &mut request => return result,
            }
        }
    }
    async fn model<T: DeserializeOwned>(
        &self,
        operation: &str,
        params: Value,
    ) -> Result<T, ModelError> {
        serde_json::from_value(self.invoke(operation, params).await.map_err(model_error)?)
            .map_err(|e| ModelError::Fatal(e.to_string()))
    }
    async fn run_agent<T: DeserializeOwned>(
        &self,
        operation: &str,
        params: Value,
        run: &RunContext,
    ) -> Result<T, AgentError> {
        let services = run.extensions.get::<ScopedAgentServices>();
        let mut host = crate::host::RunHost::new(
            run,
            services.as_ref().map(|s| s.0.as_ref()),
            self.permissions(),
            &self.declaration.id,
        );
        self.scoped_agent(operation, params, &mut host).await
    }
    async fn agent<T: DeserializeOwned>(
        &self,
        operation: &str,
        params: Value,
    ) -> Result<T, AgentError> {
        serde_json::from_value(self.invoke(operation, params).await.map_err(agent_error)?)
            .map_err(|e| AgentError::Run(e.to_string()))
    }
}
pub fn typed_fault(error: &impl Serialize, message: impl Into<String>) -> RpcFault {
    RpcFault {
        code: -32001,
        message: message.into(),
        data: serde_json::to_value(error).ok(),
    }
}
fn model_error(error: RuntimeError) -> ModelError {
    if let RuntimeError::Indeterminate(message) = error {
        return ModelError::Indeterminate(message);
    }
    if let RuntimeError::Fault(fault) = &error
        && let Some(data) = &fault.data
        && let Ok(error) = serde_json::from_value(data.clone())
    {
        return error;
    }
    ModelError::Fatal(error.to_string())
}
fn agent_error(error: RuntimeError) -> AgentError {
    if let RuntimeError::Indeterminate(message) = error {
        return AgentError::Indeterminate(message);
    }
    AgentError::Run(error.to_string())
}
#[async_trait]
impl ModelDiscovery for RemoteImplementation {
    async fn discover(&self) -> Result<Vec<ModelDescriptor>, ModelError> {
        self.model("discover", Value::Null).await
    }
}
#[async_trait]
impl TokenEstimator for RemoteImplementation {
    async fn message(&self, model: &ModelRef, message: &Message) -> Result<usize, ModelError> {
        self.model("message", json!({"model":model,"message":message}))
            .await
    }
    async fn tools(&self, model: &ModelRef, tools: &[ToolDescriptor]) -> Result<usize, ModelError> {
        self.model("tools", json!({"model":model,"tools":tools}))
            .await
    }
    async fn options(
        &self,
        model: &ModelRef,
        options: &GenerationOptions,
    ) -> Result<usize, ModelError> {
        self.model("options", json!({"model":model,"options":options}))
            .await
    }
    async fn request(&self, model: &ModelRef, request: &ChatRequest) -> Result<usize, ModelError> {
        self.model("request", json!({"model":model,"request":request}))
            .await
    }
}
macro_rules! remote_model {
    ($trait:ident, $method:ident, $request:ty, $response:ty) => {
        #[async_trait] impl $trait for RemoteImplementation {
            async fn $method(&self, model:&str, request:$request) -> Result<$response,ModelError> { self.model(stringify!($method),json!({"model":model,"request":request})).await }
        }
    };
}
remote_model!(EmbeddingModel, embed, EmbeddingRequest, EmbeddingResponse);
remote_model!(RerankModel, rerank, RerankRequest, RerankResponse);
remote_model!(
    ContentDetectionModel,
    detect,
    ContentDetectionRequest,
    ContentDetectionResponse
);
remote_model!(
    ImageGenerationModel,
    generate,
    ImageGenerationRequest,
    ImageGenerationResponse
);
remote_model!(
    SpeechRecognitionModel,
    transcribe,
    SpeechRecognitionRequest,
    SpeechRecognitionResponse
);
remote_model!(
    SpeechSynthesisModel,
    synthesize,
    SpeechSynthesisRequest,
    SpeechSynthesisResponse
);
#[async_trait]
impl VideoGenerationModel for RemoteImplementation {
    async fn submit(
        &self,
        model: &str,
        request: VideoGenerationRequest,
    ) -> Result<VideoJob, ModelError> {
        let _operation = self
            .instance
            .occupy("model_operation", "video.submit")
            .map_err(model_error)?;
        let capacity = self.instance.reserve_video_job().map_err(model_error)?;
        let job = self
            .model("submit", json!({"model":model,"request":request}))
            .await?;
        self.instance
            .bind_video_job(&self.declaration.id, job, capacity)
            .map_err(model_error)
    }
    async fn query(&self, job: &VideoJob) -> Result<VideoJob, ModelError> {
        let _operation = self
            .instance
            .occupy("model_operation", "video.query")
            .map_err(model_error)?;
        let original = self
            .instance
            .video_job(&self.declaration.id, &job.id)
            .map_err(model_error)?;
        let result = self.model("query", json!({"job":original})).await?;
        self.instance
            .update_video_job(&job.id, result)
            .map_err(model_error)
    }
    async fn cancel(&self, job: &VideoJob) -> Result<(), ModelError> {
        let _operation = self
            .instance
            .occupy("model_operation", "video.cancel")
            .map_err(model_error)?;
        let original = self
            .instance
            .video_job(&self.declaration.id, &job.id)
            .map_err(model_error)?;
        self.model::<()>("cancel", json!({"job":original})).await?;
        self.instance.forget_video_job(&job.id);
        Ok(())
    }
}
struct StreamingHost<'a> {
    stream: &'a dyn ChatStream,
    host: Option<crate::host::RunHost<'a>>,
    trace: Option<TraceContext>,
}
#[async_trait]
impl ScopedHost for StreamingHost<'_> {
    fn trace_context(&self) -> Option<TraceContext> {
        self.trace
            .clone()
            .or_else(|| self.host.as_ref().and_then(ScopedHost::trace_context))
    }
    async fn call(&mut self, request: RpcRequest) -> Result<Value, RpcFault> {
        if !request.method.starts_with("host.stream.") {
            return match self.host.as_mut() {
                Some(host) => host.call(request).await,
                None => Err(RpcFault::new(
                    "model Host API is unavailable outside an execution scope",
                )),
            };
        }
        let text = request.params["text"]
            .as_str()
            .ok_or_else(|| RpcFault::new("missing stream text"))?;
        match request.method.as_str() {
            "host.stream.text" => self.stream.text_delta(text).await,
            "host.stream.reasoning" => self.stream.reasoning_delta(text).await,
            _ => return Err(RpcFault::new("stream capability not granted")),
        }
        Ok(Value::Null)
    }
}
#[async_trait]
impl ChatModel for RemoteImplementation {
    async fn generate(
        &self,
        model_id: &str,
        request: ChatRequest,
        stream: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        let run = request.extensions.get::<ScopedRunContext>();
        let services = run
            .as_ref()
            .and_then(|run| run.0.extensions.get::<ScopedAgentServices>());
        let host = run.as_ref().map(|run| {
            crate::host::RunHost::new(
                &run.0,
                services.as_ref().map(|s| s.0.as_ref()),
                self.permissions(),
                &self.declaration.id,
            )
        });
        let trace = request
            .extensions
            .get::<TraceContext>()
            .map(|trace| (*trace).clone());
        let value = self
            .invoke_scoped(
                "generate",
                json!({"model":model_id,"request":request}),
                &mut StreamingHost {
                    stream,
                    host,
                    trace,
                },
            )
            .await
            .map_err(model_error)?;
        serde_json::from_value(value).map_err(|e| ModelError::Fatal(e.to_string()))
    }
}
fn run_identity(run: &RunContext) -> Value {
    json!({"run_id":run.run_id,"execution_id":run.execution_id(),"session_id":run.session_id,"model":run.model,"limits":run.limits,"lineage":run.lineage})
}
#[async_trait]
impl ContextContributor for RemoteImplementation {
    fn kind(&self) -> ContextSourceKind {
        if self.declaration.extension == "framework.context-provider" {
            ContextSourceKind::Instruction
        } else {
            ContextSourceKind::Data
        }
    }
    fn block_id(&self) -> &str {
        self.declaration.metadata["block_id"]
            .as_str()
            .unwrap_or(&self.declaration.id)
    }
    async fn contribute(&self, input: &ContextInput<'_>) -> Result<ContextBlock, AgentError> {
        if ContextContributor::kind(self) == ContextSourceKind::Instruction {
            let messages = self
                .run_agent("context", run_identity(input.run), input.run)
                .await?;
            return Ok(ContextBlock::instructions(self.block_id(), messages));
        }
        let services = input.run.extensions.get::<ScopedAgentServices>();
        let mut host = crate::host::RunHost::new(
            input.run,
            services.as_ref().map(|s| s.0.as_ref()),
            self.permissions(),
            &self.declaration.id,
        );
        host.estimator = Some(input.estimator);
        self.scoped_agent("contribute",json!({"run":run_identity(input.run),"model":input.model,"history":input.history,"budget":input.budget}),&mut host).await
    }
}

#[async_trait]
impl ModelSelectionStrategy for RemoteImplementation {
    async fn validate_async(&self, catalog: &ModelCatalog) -> Result<(), AgentError> {
        self.agent("validate", json!({"models":catalog.models()}))
            .await
    }
    async fn select(
        &self,
        input: &ModelSelectionContext<'_>,
        catalog: &ModelCatalog,
    ) -> Result<ModelRef, AgentError> {
        self.agent("select",json!({"stage":input.stage,"step":input.step,"attempt":input.attempt,"transient_failures":input.transient_failures,"previous":input.previous,"messages":input.messages,"tools":input.tools,"options":input.options,"run":run_identity(input.run),"models":catalog.models()})).await
    }
}
#[async_trait]
impl ContextBudgetPolicy for RemoteImplementation {
    async fn allocate(
        &self,
        blocks: &[ContextBlockConfig],
        available: ContextBudget,
    ) -> Result<Vec<ContextBudget>, AgentError> {
        let blocks:Vec<_>=blocks.iter().map(|block|json!({"id":block.id,"max_tokens":block.budget.max_tokens,"max_ratio_bps":block.budget.max_ratio_bps,"max_messages":block.budget.max_messages})).collect();
        self.agent("allocate", json!({"blocks":blocks,"available":available}))
            .await
    }
}
#[async_trait]
impl SkillProvider for RemoteImplementation {
    async fn catalog(&self, run: &RunContext) -> Result<SkillCatalog, AgentError> {
        self.run_agent("catalog", json!({"run":run_identity(run)}), run)
            .await
    }
    async fn load(&self, run: &RunContext, name: &str) -> Result<SkillDetail, AgentError> {
        self.run_agent("load", json!({"run":run_identity(run),"name":name}), run)
            .await
    }
    async fn read_resource(
        &self,
        run: &RunContext,
        name: &str,
        path: &str,
    ) -> Result<SkillResource, AgentError> {
        self.run_agent(
            "read_resource",
            json!({"run":run_identity(run),"name":name,"path":path}),
            run,
        )
        .await
    }
    fn release(&self, run: &RunContext) {
        let implementation = self.clone();
        let identity = run_identity(run);
        if let Ok(runtime) = tokio::runtime::Handle::try_current()
            && let Ok(lease) = self.instance.occupy("resource_release", "skills.release")
        {
            runtime.spawn(async move {
                let _lease = lease;
                let _ = implementation
                    .invoke("release", json!({"run":identity}))
                    .await;
            });
        }
    }
}
fn state_error(error: RuntimeError) -> StateError {
    if let RuntimeError::Indeterminate(message) = error {
        return StateError::Indeterminate(message);
    }
    if let RuntimeError::Fault(fault) = &error
        && let Some(data) = &fault.data
        && let Ok(error) = serde_json::from_value(data.clone())
    {
        return error;
    }
    StateError::Backend(error.to_string())
}
fn memory_error(error: RuntimeError) -> MemoryError {
    if let RuntimeError::Indeterminate(message) = error {
        return MemoryError::Indeterminate(message);
    }
    if let RuntimeError::Fault(fault) = &error
        && let Some(data) = &fault.data
        && let Ok(error) = serde_json::from_value(data.clone())
    {
        return error;
    }
    MemoryError::Backend(error.to_string())
}
impl RemoteImplementation {
    async fn state<T: DeserializeOwned>(
        &self,
        operation: &str,
        params: Value,
    ) -> Result<T, StateError> {
        serde_json::from_value(self.invoke(operation, params).await.map_err(state_error)?)
            .map_err(|e| StateError::Backend(e.to_string()))
    }
    async fn memory_result<T: DeserializeOwned>(
        &self,
        operation: &str,
        params: Value,
    ) -> Result<T, MemoryError> {
        serde_json::from_value(self.invoke(operation, params).await.map_err(memory_error)?)
            .map_err(|e| MemoryError::Backend(e.to_string()))
    }
}
#[async_trait]
impl StateStore for RemoteImplementation {
    async fn create(
        &self,
        checkpoint: Checkpoint,
        previous_run: Option<RunId>,
    ) -> Result<Checkpoint, StateError> {
        self.state(
            "create",
            json!({"checkpoint":checkpoint,"previous_run":previous_run}),
        )
        .await
    }
    async fn load(&self, run_id: RunId) -> Result<Checkpoint, StateError> {
        self.state("load", json!({"run_id":run_id})).await
    }
    async fn latest(&self, session_id: &str) -> Result<Option<Checkpoint>, StateError> {
        self.state("latest", json!({"session_id":session_id})).await
    }
    async fn latest_scope(
        &self,
        session_id: &str,
        scope: &str,
    ) -> Result<Option<Checkpoint>, StateError> {
        self.state(
            "latest_scope",
            json!({"session_id":session_id,"scope":scope}),
        )
        .await
    }
    async fn claim(
        &self,
        run_id: RunId,
        owner: RunId,
        ttl: Duration,
    ) -> Result<ExecutionLease, StateError> {
        let lease = self
            .state("claim", json!({"run_id":run_id,"owner":owner,"ttl":ttl}))
            .await?;
        self.instance
            .track_state_lease(&self.declaration.id, &lease)
            .map_err(|e| StateError::Backend(e.to_string()))?;
        Ok(lease)
    }
    async fn renew(
        &self,
        lease: &ExecutionLease,
        ttl: Duration,
    ) -> Result<ExecutionLease, StateError> {
        if !self.instance.owns_state_lease(&self.declaration.id, lease) {
            return Err(StateError::LeaseLost);
        }
        self.state("renew", json!({"lease":lease,"ttl":ttl})).await
    }
    async fn commit(
        &self,
        lease: &ExecutionLease,
        expected_revision: u64,
        next: Checkpoint,
    ) -> Result<Checkpoint, StateError> {
        if !self.instance.owns_state_lease(&self.declaration.id, lease) {
            return Err(StateError::LeaseLost);
        }
        self.state(
            "commit",
            json!({"lease":lease,"expected_revision":expected_revision,"next":next}),
        )
        .await
    }
    async fn release(&self, lease: &ExecutionLease) -> Result<(), StateError> {
        if !self.instance.owns_state_lease(&self.declaration.id, lease) {
            return Err(StateError::LeaseLost);
        }
        let result = self.state("release", json!({"lease":lease})).await;
        if result.is_ok() || matches!(result, Err(StateError::LeaseLost)) {
            self.instance
                .forget_state_lease(&self.declaration.id, lease);
        }
        result
    }
}
#[async_trait]
impl InteractionStore for RemoteImplementation {
    async fn close_run(&self, session_id: &str, run_id: RunId) -> Result<(), StateError> {
        self.state(
            "close_run",
            json!({"session_id":session_id,"run_id":run_id}),
        )
        .await
    }
    async fn publish(&self, record: &InteractionRecord) -> Result<(), StateError> {
        self.state("publish", json!({"record":record})).await
    }
    async fn publish_many(&self, records: &[InteractionRecord]) -> Result<(), StateError> {
        if records.is_empty() {
            return Ok(());
        }
        self.state("publish_many", json!({"records":records})).await
    }
    async fn get(&self, session_id: &str, id: RunId) -> Result<InteractionRecord, StateError> {
        self.state("get", json!({"session_id":session_id,"id":id}))
            .await
    }
    async fn get_many(
        &self,
        session_id: &str,
        ids: &[RunId],
    ) -> Result<Vec<InteractionRecord>, StateError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        self.state("get_many", json!({"session_id":session_id,"ids":ids}))
            .await
    }
    async fn find_many(
        &self,
        session_id: &str,
        ids: &[RunId],
    ) -> Result<Vec<Option<InteractionRecord>>, StateError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        self.state("find_many", json!({"session_id":session_id,"ids":ids}))
            .await
    }
    async fn pending(&self, session_id: &str) -> Result<Vec<InteractionRecord>, StateError> {
        self.state("pending", json!({"session_id":session_id}))
            .await
    }
    async fn answer(
        &self,
        session_id: &str,
        response: &InteractionResponse,
    ) -> Result<InteractionRecord, AgentError> {
        self.agent(
            "answer",
            json!({"session_id":session_id,"response":response}),
        )
        .await
    }
}
#[async_trait]
impl LongTermMemory for RemoteImplementation {
    async fn get(&self, namespace: &str, id: &str) -> Result<MemoryEntry, MemoryError> {
        self.memory_result("get", json!({"namespace":namespace,"id":id}))
            .await
    }
    async fn search(
        &self,
        namespace: &str,
        query: &MemoryQuery,
    ) -> Result<Vec<MemoryEntry>, MemoryError> {
        self.memory_result("search", json!({"namespace":namespace,"query":query}))
            .await
    }
    async fn save(
        &self,
        namespace: &str,
        value: &MemoryWrite,
        expected: Option<&str>,
    ) -> Result<MemoryEntry, MemoryError> {
        self.memory_result(
            "save",
            json!({"namespace":namespace,"value":value,"expected":expected}),
        )
        .await
    }
    async fn delete(&self, namespace: &str, id: &str, expected: &str) -> Result<(), MemoryError> {
        self.memory_result(
            "delete",
            json!({"namespace":namespace,"id":id,"expected":expected}),
        )
        .await
    }
}
#[async_trait]
impl ToolPolicy for RemoteImplementation {
    async fn evaluate(
        &self,
        input: &ToolInvocationContext<'_>,
    ) -> Result<ToolPolicyDecision, AgentError> {
        self.agent("evaluate",json!({"invocation_id":input.invocation_id,"call":input.call,"descriptor":input.descriptor,"run":run_identity(input.run)})).await
    }
}

fn conversation_error(error: RuntimeError) -> ConversationError {
    if let RuntimeError::Indeterminate(message) = error {
        return ConversationError::Indeterminate(message);
    }
    if let RuntimeError::Fault(fault) = &error
        && let Some(data) = &fault.data
        && let Ok(error) = serde_json::from_value(data.clone())
    {
        return error;
    }
    ConversationError::Other(error.to_string())
}
struct RemoteConversation(Arc<RemoteResource>);
#[async_trait]
impl ConversationStore for RemoteImplementation {
    async fn open(
        &self,
        session_id: &str,
    ) -> Result<Box<dyn ConversationSession>, ConversationError> {
        self.open_branch(&ConversationKey::new(session_id)).await
    }
    async fn open_branch(
        &self,
        key: &ConversationKey,
    ) -> Result<Box<dyn ConversationSession>, ConversationError> {
        let value = self
            .invoke("open", json!({"key":key}))
            .await
            .map_err(conversation_error)?;
        let handle =
            serde_json::from_value(value).map_err(|e| ConversationError::Other(e.to_string()))?;
        Ok(Box::new(RemoteConversation(
            RemoteResource::new(self.clone(), handle).map_err(conversation_error)?,
        )))
    }
}
#[async_trait]
impl ConversationSession for RemoteConversation {
    async fn snapshot(&mut self) -> Result<ConversationSnapshot, ConversationError> {
        serde_json::from_value(
            self.0
                .call("snapshot", Value::Null)
                .await
                .map_err(conversation_error)?,
        )
        .map_err(|e| ConversationError::Other(e.to_string()))
    }
    async fn append(&mut self, messages: &[Message]) -> Result<(), ConversationError> {
        self.0
            .call("append", json!({"messages":messages}))
            .await
            .map_err(conversation_error)?;
        Ok(())
    }
    async fn append_at(
        &mut self,
        revision: u64,
        messages: &[Message],
    ) -> Result<(), ConversationError> {
        self.0
            .call(
                "append_at",
                json!({"revision":revision,"messages":messages}),
            )
            .await
            .map_err(conversation_error)?;
        Ok(())
    }
    async fn finish(&mut self, outcome: ConversationOutcome) -> Result<(), ConversationError> {
        self.0
            .call("finish", json!({"outcome":outcome}))
            .await
            .map_err(conversation_error)?;
        self.0.close().await.map_err(conversation_error)
    }
}
struct RemoteRealtime {
    resource: Arc<RemoteResource>,
    cancellation: tokio_util::sync::CancellationToken,
}
impl Drop for RemoteRealtime {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}
#[async_trait]
impl RealtimeModel for RemoteImplementation {
    async fn connect(
        &self,
        model: &str,
        request: RealtimeRequest,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<Arc<dyn RealtimeSession>, ModelError> {
        let handle = self
            .model("connect", json!({"model":model,"request":request}))
            .await?;
        let resource = RemoteResource::new(self.clone(), handle).map_err(model_error)?;
        let weak = Arc::downgrade(&resource);
        let cancelled = cancellation.clone();
        tokio::spawn(async move {
            cancelled.cancelled().await;
            if let Some(resource) = weak.upgrade() {
                let _ = resource.close().await;
            }
        });
        Ok(Arc::new(RemoteRealtime {
            resource,
            cancellation,
        }))
    }
}
#[async_trait]
impl RealtimeSession for RemoteRealtime {
    async fn send(&self, input: RealtimeInput) -> Result<(), ModelError> {
        self.resource
            .call("send", json!({"input":input}))
            .await
            .map_err(model_error)?;
        Ok(())
    }
    async fn next(&self) -> Result<Option<RealtimeEvent>, ModelError> {
        serde_json::from_value(
            self.resource
                .call("next", Value::Null)
                .await
                .map_err(model_error)?,
        )
        .map_err(|e| ModelError::Fatal(e.to_string()))
    }
    async fn close(&self) -> Result<(), ModelError> {
        self.cancellation.cancel();
        self.resource.close().await.map_err(model_error)
    }
}
#[derive(serde::Deserialize)]
struct RemoteToolDescriptor {
    descriptor: ToolDescriptor,
    #[serde(default = "default_true")]
    exposed: bool,
    #[serde(default)]
    parallel_safe: bool,
    /// Capability group carried across the process hop so remote tools keep
    /// their `domainCode` listing at the gateway.
    #[serde(default)]
    domain_code: Option<String>,
    /// Category declared by the remote tool provider.
    #[serde(default)]
    category: Option<ToolCategory>,
}
fn default_true() -> bool {
    true
}
#[derive(serde::Deserialize)]
struct RemoteToolLease {
    handle: RemoteResourceHandle,
    tools: Vec<RemoteToolDescriptor>,
}
struct RemoteTool {
    implementation: RemoteImplementation,
    id: String,
    resource: Arc<RemoteResource>,
}
#[async_trait]
impl Tool for RemoteTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let services = context.extensions.get::<ScopedAgentServices>();
        let mut host = crate::host::RunHost::new(
            context,
            services.as_ref().map(|s| s.0.as_ref()),
            self.implementation.permissions(),
            &self.implementation.declaration.id,
        );
        let value=self.implementation.invoke_scoped("call",json!({"tool":self.id,"arguments":arguments,"run":run_identity(context),"lease":self.resource.handle()}),&mut host).await.map_err(tool_error)?;
        serde_json::from_value(value).map_err(|e| ToolError::Indeterminate(e.to_string()))
    }
}
fn tool_error(error: RuntimeError) -> ToolError {
    if let RuntimeError::Fault(fault) = &error {
        if let Some(data) = &fault.data
            && let Ok(error) = serde_json::from_value(data.clone())
        {
            return error;
        }
        return ToolError::Failed(fault.message.clone());
    }
    ToolError::Indeterminate(error.to_string())
}
#[async_trait]
impl ToolProvider for RemoteImplementation {
    async fn lease(&self, context: &RunContext) -> Result<ToolLease, AgentError> {
        let lease: RemoteToolLease = self
            .run_agent("lease", json!({"run":run_identity(context)}), context)
            .await?;
        let resource = RemoteResource::new(self.clone(), lease.handle).map_err(agent_error)?;
        let tools = lease
            .tools
            .into_iter()
            .map(|tool| ToolBinding {
                executor: Arc::new(RemoteTool {
                    implementation: self.clone(),
                    id: tool.descriptor.id.clone(),
                    resource: resource.clone(),
                }),
                descriptor: tool.descriptor,
                exposed: tool.exposed,
                parallel_safe: tool.parallel_safe,
                domain_code: tool.domain_code,
                category: tool.category,
                source: ToolSource {
                    provider_id: Some(self.declaration.id.clone()),
                    pack_id: Some(self.instance.identity.package.clone()),
                    ..Default::default()
                },
            })
            .collect();
        Ok(ToolLease::new(tools).with_shutdown(move || async move {
            resource.close().await.map_err(|e| e.to_string())
        }))
    }
}

impl RemoteImplementation {
    fn flag(&self, name: &str, default: bool) -> bool {
        self.declaration.metadata[name].as_bool().unwrap_or(default)
    }
    fn permissions(&self) -> &[String] {
        &self.instance.pack().manifest.permissions
    }
    async fn scoped_agent<T: DeserializeOwned>(
        &self,
        operation: &str,
        params: Value,
        host: &mut dyn ScopedHost,
    ) -> Result<T, AgentError> {
        serde_json::from_value(
            self.invoke_scoped(operation, params, host)
                .await
                .map_err(agent_error)?,
        )
        .map_err(|e| AgentError::Run(e.to_string()))
    }
}
#[async_trait]
impl AgentLoop for RemoteImplementation {
    fn supports_steering(&self) -> bool {
        self.flag("supports_steering", false)
    }
    fn supports_resume(&self) -> bool {
        self.flag("supports_resume", false)
    }
    fn requires_interactions(&self) -> bool {
        self.flag("requires_interactions", false)
    }
    fn supports_tool_policy(&self) -> bool {
        self.flag("supports_tool_policy", false)
    }
    fn requires_state(&self) -> bool {
        self.flag("requires_state", false)
    }
    fn requires_model(&self) -> bool {
        self.flag("requires_model", false)
    }
    fn cooperative_cancellation(&self) -> bool {
        true
    }
    async fn validate_services_async(&self, services: &AgentServices) -> Result<(), AgentError> {
        self.agent("validate",json!({"models":services.models.as_ref().map(|m|m.catalog().models()),"state":services.state.is_some(),"interactions":services.interactions.is_some()})).await
    }
    async fn run(
        &self,
        request: &RunRequest,
        context: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        self.scoped_agent("run",json!({"request":request,"resume":request.resume.as_ref().map(|r|json!({"run_id":r.run_id,"response":r.response})),"run":run_identity(context)}),&mut crate::host::RunHost::new(context,Some(services),self.permissions(), &self.declaration.id)).await
    }
}
#[async_trait]
impl ContextReducer for RemoteImplementation {
    async fn reduce(&self, input: &ContextReductionInput<'_>) -> Result<Vec<Message>, AgentError> {
        let mut host =
            crate::host::RunHost::new(input.run, None, self.permissions(), &self.declaration.id);
        host.models = Some(input.models);
        host.estimator = Some(input.estimator);
        let mut params = json!({"block":input.block,"budget":input.budget,"model":input.model,"history":input.history,"run":run_identity(input.run),"limits":input.limits,"overflow_recoveries":input.overflow_recoveries});
        // Preserve the legacy wire shape when no compression was requested.
        if let Some(directive) = input.compression {
            params["compression"] = json!(directive);
        }
        self.scoped_agent("reduce", params, &mut host).await
    }
}
#[async_trait]
impl ContextAssembler for RemoteImplementation {
    async fn assemble(
        &self,
        input: &ContextAssemblyInput<'_>,
    ) -> Result<ContextWindow, AgentError> {
        let mut host =
            crate::host::RunHost::new(input.run, None, self.permissions(), &self.declaration.id);
        host.models = Some(input.models);
        host.estimator = Some(input.estimator);
        self.scoped_agent("assemble",json!({"stage":input.stage,"attempt":input.attempt,"step":input.step,"model":input.model,"budget":input.budget,"history":input.history,"instructions":input.instructions,"tools":input.tools,"options":input.options,"run":run_identity(input.run),"limits":input.limits,"overflow_recoveries":input.overflow_recoveries}),&mut host).await
    }
}
struct RunContinuation<'a> {
    next: Option<RunNext<'a>>,
    host: crate::host::RunHost<'a>,
}
#[async_trait]
impl ScopedHost for RunContinuation<'_> {
    async fn call(&mut self, request: RpcRequest) -> Result<Value, RpcFault> {
        if request.method == "host.next" {
            let next = self
                .next
                .take()
                .ok_or_else(|| RpcFault::new("continuation already consumed"))?;
            return serde_json::to_value(
                next.run().await.map_err(|e| RpcFault::new(e.to_string()))?,
            )
            .map_err(|e| RpcFault::new(e.to_string()));
        }
        self.host.dispatch(request).await
    }
}
#[async_trait]
impl RunMiddleware for RemoteImplementation {
    async fn intercept(
        &self,
        request: &RunRequest,
        context: &RunContext,
        next: RunNext<'_>,
    ) -> Result<RunResult, AgentError> {
        let services = next.services;
        self.scoped_agent(
            "intercept",
            json!({"request":request,"run":run_identity(context),"continuation":"host.next"}),
            &mut RunContinuation {
                next: Some(next),
                host: crate::host::RunHost::new(
                    context,
                    Some(services),
                    self.permissions(),
                    &self.declaration.id,
                ),
            },
        )
        .await
    }
}
struct ModelContinuation<'a> {
    next: Option<ModelNext<'a>>,
    host: crate::host::RunHost<'a>,
    stream: &'a dyn ChatStream,
    context: &'a ModelInvocationContext<'a>,
}
#[async_trait]
impl ScopedHost for ModelContinuation<'_> {
    async fn call(&mut self, request: RpcRequest) -> Result<Value, RpcFault> {
        if request.method == "host.next" {
            let next = self
                .next
                .take()
                .ok_or_else(|| RpcFault::new("continuation already consumed"))?;
            let mut candidate: ChatRequest =
                serde_json::from_value(request.params).map_err(|e| RpcFault::new(e.to_string()))?;
            candidate.extensions = self.host.run.extensions.clone();
            return serde_json::to_value(
                next.run(candidate, self.context)
                    .await
                    .map_err(|e| typed_fault(&e, e.to_string()))?,
            )
            .map_err(|e| RpcFault::new(e.to_string()));
        }
        if request.method.starts_with("host.stream.") {
            return StreamingHost {
                stream: self.stream,
                host: None,
                trace: None,
            }
            .call(request)
            .await;
        }
        self.host.dispatch(request).await
    }
}
#[async_trait]
impl ModelMiddleware for RemoteImplementation {
    async fn intercept(
        &self,
        request: ChatRequest,
        context: &ModelInvocationContext<'_>,
        next: ModelNext<'_>,
    ) -> Result<ChatResponse, ModelError> {
        let stream = next.stream;
        let mut host =
            crate::host::RunHost::new(context.run, None, self.permissions(), &self.declaration.id);
        host.estimator = Some(next.estimator);
        let value=self.invoke_scoped("intercept",json!({"request":request,"model":context.model,"stage":context.stage,"step":context.step,"attempt":context.attempt,"run":run_identity(context.run),"continuation":"host.next"}),&mut ModelContinuation{next:Some(next),host,stream,context}).await.map_err(model_error)?;
        serde_json::from_value(value).map_err(|e| ModelError::Fatal(e.to_string()))
    }
}
struct ToolContinuation<'a> {
    next: Option<ToolNext<'a>>,
    host: crate::host::RunHost<'a>,
}
#[async_trait]
impl ScopedHost for ToolContinuation<'_> {
    async fn call(&mut self, request: RpcRequest) -> Result<Value, RpcFault> {
        if request.method == "host.next" {
            let next = self
                .next
                .take()
                .ok_or_else(|| RpcFault::new("continuation already consumed"))?;
            return serde_json::to_value(
                next.run()
                    .await
                    .map_err(|e| typed_fault(&e, e.to_string()))?,
            )
            .map_err(|e| RpcFault::new(e.to_string()));
        }
        self.host.dispatch(request).await
    }
}
#[async_trait]
impl ToolMiddleware for RemoteImplementation {
    async fn intercept(
        &self,
        context: &ToolInvocationContext<'_>,
        next: ToolNext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let value=self.invoke_scoped("intercept",json!({"call":context.call,"descriptor":context.descriptor,"invocation_id":context.invocation_id,"run":run_identity(context.run),"continuation":"host.next"}),&mut ToolContinuation{next:Some(next),host:crate::host::RunHost::new(context.run,None,self.permissions(), &self.declaration.id)}).await.map_err(tool_error)?;
        serde_json::from_value(value).map_err(|e| ToolError::Indeterminate(e.to_string()))
    }
}
#[async_trait]
impl RunLifecycleHook for RemoteImplementation {
    async fn on_started(&self, context: &RunLifecycleContext<'_>) {
        let _=self.invoke("on_started",json!({"run_id":context.run_id,"execution_id":context.execution_id,"session_id":context.session_id})).await;
    }
    async fn on_finished(
        &self,
        context: &RunLifecycleContext<'_>,
        result: &Result<RunResult, AgentError>,
    ) {
        let _=self.invoke("on_finished",json!({"run_id":context.run_id,"execution_id":context.execution_id,"session_id":context.session_id,"result":result.as_ref().ok(),"error":result.as_ref().err().map(ToString::to_string)})).await;
    }
}
#[async_trait]
impl EventSink for RemoteImplementation {
    async fn on_event(&self, context: &RunContext, event: &AgentEvent) {
        // Only explicitly selected event categories cross this observation path.
        let value = serde_json::to_value(event).unwrap_or(Value::Null);
        let category = value
            .as_object()
            .and_then(|m| m.keys().next())
            .map(String::as_str)
            .unwrap_or("");
        if self.declaration.metadata["subscriptions"]
            .as_array()
            .is_some_and(|categories| categories.iter().any(|c| c.as_str() == Some(category)))
        {
            let _ = self
                .invoke(
                    "on_event",
                    json!({"run":run_identity(context),"event":value}),
                )
                .await;
        }
    }
}
