use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, atomic::AtomicU64},
};

use eliteforge_agent_plugin_kernel::{Plugin, PluginContainer};
use futures::FutureExt;
use tokio::sync::{Notify, broadcast, oneshot};
use tokio_util::sync::CancellationToken;

use crate::*;

pub struct AgentBuilder {
    plugins: Vec<Arc<dyn Plugin>>,
    capability_packs: Vec<Arc<dyn CapabilityPack>>,
    default_models: bool,
}
impl Default for AgentBuilder {
    fn default() -> Self {
        Self {
            plugins: Vec::new(),
            capability_packs: Vec::new(),
            default_models: true,
        }
    }
}

impl AgentBuilder {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn default_models(mut self, enabled: bool) -> Self {
        self.default_models = enabled;
        self
    }
    pub fn plugin(mut self, plugin: impl Plugin + 'static) -> Self {
        self.plugins.push(Arc::new(plugin));
        self
    }
    pub fn plugin_arc(mut self, plugin: Arc<dyn Plugin>) -> Self {
        self.plugins.push(plugin);
        self
    }

    /// Select a configured capability pack for this Agent's dependency graph.
    pub fn capability_pack(mut self, pack: impl CapabilityPack + 'static) -> Self {
        self.capability_packs.push(Arc::new(pack));
        self
    }

    /// Add a group of configured plugins to this Agent's dependency graph.
    /// Duplicate IDs and dependency errors are reported by `build`, before installation.
    pub fn plugin_pack(mut self, pack: impl PluginPack) -> Self {
        self.plugins.extend(pack.into_plugins());
        self
    }

    pub async fn build(mut self) -> Result<Agent, AgentError> {
        let mut occupied_plugin_ids = HashSet::with_capacity(self.plugins.len() + 1);
        for (index, plugin) in self.plugins.iter().enumerate() {
            let id = plugin.id().to_owned();
            if id.trim().is_empty() {
                return Err(PluginError::EmptyPluginId { index }.into());
            }
            if !occupied_plugin_ids.insert(id.clone()) {
                return Err(PluginError::DuplicatePluginId { id }.into());
            }
        }
        // DefaultsPlugin joins the same graph below. Its ID must be reserved
        // before capability factories run, but cannot satisfy a capability
        // dependency because DefaultsPlugin is ordered after all capabilities.
        let defaults_plugin_id = "builtin.framework-defaults".to_owned();
        if occupied_plugin_ids.contains(&defaults_plugin_id) {
            return Err(PluginError::DuplicatePluginId {
                id: defaults_plugin_id,
            }
            .into());
        }
        let reserved_plugin_ids = HashSet::from([defaults_plugin_id]);
        let capability_plugins = resolve_capability_packs_with_plugins(
            &self.capability_packs,
            &self.plugins,
            &reserved_plugin_ids,
        )?;
        self.plugins.extend(capability_plugins);
        let after = self.plugins.iter().map(|p| p.id().to_owned()).collect();
        self.plugins.push(Arc::new(crate::plugins::DefaultsPlugin {
            after,
            default_models: self.default_models,
        }));
        let mut container = PluginContainer::build(self.plugins).await?;
        let (agent_loop, services, sinks, outputs) = match (|| -> Result<_, AgentError> {
            let registry = container.services();
            let agent_loop = registry
                .get::<LoopService>()
                .ok_or(AgentError::MissingService("AgentLoop"))?
                .0
                .clone();
            let connections: Vec<_> = registry
                .all::<ModelConnectionService>()
                .iter()
                .map(|service| service.0.clone())
                .collect();
            let strategy = registry
                .get::<ModelStrategyService>()
                .map(|service| service.0.clone())
                .or_else(|| {
                    registry
                        .get::<ModelRouting>()
                        .filter(|r| {
                            r.defaults.contains_key(&ModelOperation::Chat)
                                || r.stages.keys().any(|(op, _)| *op == ModelOperation::Chat)
                        })
                        .map(|r| {
                            Arc::new(CatalogModelStrategy((*r).clone()))
                                as Arc<dyn ModelSelectionStrategy>
                        })
                });
            if registry.get::<ContextAssemblerService>().is_some()
                && (registry.get::<ContextLayoutService>().is_some()
                    || registry.get::<ContextBudgetPolicyService>().is_some()
                    || registry.get::<ContextReducerService>().is_some()
                    || !registry.all::<ContextContributorService>().is_empty())
            {
                return Err(AgentError::InvalidConfiguration(
                    "configure sources, layout and reducers inside a custom ContextAssembler; separate context plugins would be ignored".into(),
                ));
            }
            if registry.get::<ContextReducerService>().is_some()
                && registry
                    .get::<ContextLayoutService>()
                    .is_some_and(|s| !s.0.0.is_empty())
            {
                return Err(AgentError::InvalidConfiguration(
                    "an explicit ContextLayout must configure its history reducer on the history block".into(),
                ));
            }
            let models: Option<Arc<dyn ChatExecutor>> = if connections.is_empty() {
                if strategy.is_some() {
                    return Err(AgentError::MissingService("ModelConnection"));
                }
                None
            } else {
                Some(Arc::new(
                    ModelInvoker::new(ModelCatalog::new(connections)?, strategy)?
                        .with_middleware(
                            registry
                                .all::<ModelMiddlewareService>()
                                .iter()
                                .map(|s| s.0.clone())
                                .collect(),
                        )
                        .with_assembler(match registry.get::<ContextAssemblerService>() {
                            Some(service) => service.0.clone(),
                            None => {
                                let mut assembler = DefaultContextAssembler::new(
                                    registry
                                        .get::<ContextLayoutService>()
                                        .map_or_else(ContextLayout::default, |s| s.0.clone()),
                                    registry
                                        .all::<ContextContributorService>()
                                        .iter()
                                        .map(|s| s.0.clone())
                                        .collect(),
                                )?;
                                if let Some(policy) = registry.get::<ContextBudgetPolicyService>() {
                                    assembler = assembler.with_policy(policy.0.clone());
                                }
                                if let Some(reducer) = registry.get::<ContextReducerService>() {
                                    assembler = assembler.with_history_reducer(reducer.0.clone());
                                }
                                Arc::new(assembler)
                            }
                        }),
                ))
            };
            if agent_loop.requires_model() && models.is_none() {
                return Err(AgentError::MissingService("ChatModel"));
            }
            let services = AgentServices {
                operations: Arc::new(crate::capability_execution::ModelClient::from_registry(
                    registry,
                )?),
                interactions: registry
                    .get::<InteractionStoreService>()
                    .map(|s| InteractionManager(s.0.clone())),
                long_term_memory: registry.get::<LongTermMemoryService>().map(|s| s.0.clone()),
                lifecycle_hooks: registry
                    .all::<RunLifecycleHookService>()
                    .iter()
                    .map(|s| s.0.clone())
                    .collect(),
                node_middleware: registry
                    .all::<WorkflowNodeMiddlewareService>()
                    .iter()
                    .map(|s| s.0.clone())
                    .collect(),
                workflow_nodes: WorkflowNodeRegistry::new(
                    registry
                        .all::<WorkflowNodeService>()
                        .iter()
                        .map(|s| s.0.clone())
                        .collect(),
                )?,
                models,
                tool_invoker: Arc::new(ToolInvoker::new(
                    registry
                        .all::<ToolMiddlewareService>()
                        .iter()
                        .map(|s| s.0.clone())
                        .collect(),
                )),
                run_middleware: registry
                    .all::<RunMiddlewareService>()
                    .iter()
                    .map(|s| s.0.clone())
                    .collect(),
                state: registry.get::<StateStoreService>().map(|s| s.0.clone()),
                tool_policy: registry
                    .all::<ToolPolicyService>()
                    .iter()
                    .map(|s| s.0.clone())
                    .collect(),
                conversations: registry
                    .get::<ConversationStoreService>()
                    .ok_or(AgentError::MissingService("ConversationStore"))?
                    .0
                    .clone(),
                tools: registry
                    .all::<ToolProviderService>()
                    .iter()
                    .map(|s| s.0.clone())
                    .collect(),
                context: registry
                    .all::<ContextService>()
                    .iter()
                    .map(|s| s.0.clone())
                    .collect(),
            };
            if agent_loop.requires_interactions() && services.interactions.is_none() {
                return Err(AgentError::MissingService("InteractionStore"));
            }
            if !services.tool_policy.is_empty() && !agent_loop.supports_tool_policy() {
                return Err(AgentError::InvalidConfiguration(
                    "installed loop does not enforce tool policies".into(),
                ));
            }
            if agent_loop.requires_state() && services.state.is_none() {
                return Err(AgentError::MissingService("StateStore"));
            }
            agent_loop.validate_services(&services)?;
            let sinks = registry
                .all::<EventService>()
                .iter()
                .map(|s| s.0.clone())
                .collect();
            let outputs: Vec<_> = registry
                .all::<RunOutputService>()
                .iter()
                .map(|s| s.0.clone())
                .collect();
            let mut types = std::collections::HashSet::new();
            for output in &outputs {
                if !types.insert(output.output_type()) {
                    return Err(AgentError::InvalidConfiguration(
                        "multiple plugins provide the same run output type".into(),
                    ));
                }
            }
            Ok((agent_loop, services, sinks, outputs))
        })() {
            Ok(services) => services,
            Err(error) => {
                container
                    .close_async(std::time::Duration::from_secs(5))
                    .await;
                return Err(error);
            }
        };
        Ok(Agent {
            operations: services.operations.clone(),
            inner: Arc::new(AgentInner {
                id: RunId::new_v4(),
                cleanup: Mutex::new(CleanupState {
                    container: Some(container),
                    started: false,
                    complete: false,
                    report: eliteforge_agent_plugin_kernel::ShutdownReport::default(),
                }),
                agent_loop,
                services,
                sinks,
                outputs,
                runs: Mutex::new(RunRegistry {
                    closed: false,
                    active: HashMap::new(),
                    operations: HashMap::new(),
                }),
                drained: Notify::new(),
                cleanup_done: Notify::new(),
            }),
        })
    }
}

struct RunRegistry {
    closed: bool,
    active: HashMap<String, CancellationToken>,
    operations: HashMap<RunId, CancellationToken>,
}

struct CleanupState {
    container: Option<PluginContainer>,
    started: bool,
    complete: bool,
    report: eliteforge_agent_plugin_kernel::ShutdownReport,
}

struct AgentInner {
    id: RunId,
    cleanup: Mutex<CleanupState>,
    agent_loop: Arc<dyn AgentLoop>,
    services: AgentServices,
    sinks: Vec<Arc<dyn EventSink>>,
    outputs: Vec<Arc<dyn RunOutputFactory>>,
    runs: Mutex<RunRegistry>,
    drained: Notify,
    cleanup_done: Notify,
}

#[derive(Clone)]
pub struct Agent {
    inner: Arc<AgentInner>,
    operations: Arc<dyn ModelOperations>,
}

/// Opaque realtime connection owned by the Agent's model runtime.
#[derive(Clone)]
pub struct RealtimeHandle {
    owner: RunId,
    session: Arc<dyn RealtimeSession>,
}

impl Agent {
    /// An Agent view with per-call model options; all views share lifecycle and services.
    pub fn with_model_options(&self, options: ModelInvocationOptions) -> Self {
        Self {
            inner: self.inner.clone(),
            operations: self.operations.with_options(options),
        }
    }

    pub fn models(&self, operation: ModelOperation) -> Vec<ModelRef> {
        self.operations.models(operation)
    }

    pub async fn embed(&self, request: EmbeddingRequest) -> Result<EmbeddingResponse, AgentError> {
        self.operations.embed(request).await
    }
    pub async fn embed_query(
        &self,
        space: &EmbeddingSpace,
        request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, AgentError> {
        self.operations.embed_query(space, request).await
    }
    pub async fn rerank(&self, request: RerankRequest) -> Result<RerankResponse, AgentError> {
        self.operations.rerank(request).await
    }
    pub async fn generate_image(
        &self,
        request: ImageGenerationRequest,
    ) -> Result<ImageGenerationResponse, AgentError> {
        self.operations.generate_image(request).await
    }
    pub async fn submit_video(
        &self,
        request: VideoGenerationRequest,
    ) -> Result<VideoJob, AgentError> {
        self.operations.submit_video(request).await
    }
    pub async fn query_video(&self, job: &VideoJob) -> Result<VideoJob, AgentError> {
        self.operations.query_video(job).await
    }
    pub async fn cancel_video(&self, job: &VideoJob) -> Result<(), AgentError> {
        self.operations.cancel_video(job).await
    }
    pub async fn transcribe(
        &self,
        request: SpeechRecognitionRequest,
    ) -> Result<SpeechRecognitionResponse, AgentError> {
        self.operations.transcribe(request).await
    }
    pub async fn synthesize(
        &self,
        request: SpeechSynthesisRequest,
    ) -> Result<SpeechSynthesisResponse, AgentError> {
        self.operations.synthesize(request).await
    }
    pub async fn connect_realtime(
        &self,
        request: RealtimeRequest,
    ) -> Result<RealtimeHandle, AgentError> {
        Ok(RealtimeHandle {
            owner: self.inner.id,
            session: self.operations.connect_realtime(request).await?,
        })
    }
    fn check_realtime_owner(&self, handle: &RealtimeHandle) -> Result<(), AgentError> {
        if handle.owner != self.inner.id {
            return Err(AgentError::InvalidConfiguration(
                "realtime session belongs to a different Agent".into(),
            ));
        }
        Ok(())
    }
    pub async fn send_realtime(
        &self,
        handle: &RealtimeHandle,
        input: RealtimeInput,
    ) -> Result<(), AgentError> {
        self.check_realtime_owner(handle)?;
        Ok(handle.session.send(input).await?)
    }
    pub async fn next_realtime(
        &self,
        handle: &RealtimeHandle,
    ) -> Result<Option<RealtimeEvent>, AgentError> {
        self.check_realtime_owner(handle)?;
        Ok(handle.session.next().await?)
    }
    pub async fn close_realtime(&self, handle: &RealtimeHandle) -> Result<(), AgentError> {
        self.check_realtime_owner(handle)?;
        Ok(handle.session.close().await?)
    }

    /// Dispatch an extension request using this Agent's installed service.
    pub async fn execute<O: AgentOperation>(&self, operation: O) -> Result<O::Output, AgentError> {
        let id = RunId::new_v4();
        let cancellation = CancellationToken::new();
        {
            let mut runs = self.inner.runs.lock().unwrap_or_else(|e| e.into_inner());
            if runs.closed {
                return Err(AgentError::Closed);
            }
            runs.operations.insert(id, cancellation.clone());
        }
        let _registration = OperationRegistration {
            inner: self.inner.clone(),
            id,
        };
        let services = {
            let state = self.inner.cleanup.lock().unwrap_or_else(|e| e.into_inner());
            let registry = state
                .container
                .as_ref()
                .ok_or(AgentError::Closed)?
                .services();
            registry
                .get::<O::Service>()
                .map(|service| vec![service])
                .unwrap_or_else(|| registry.all::<O::Service>())
        };
        let service =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| operation.select(services)))
                .map_err(|_| AgentError::Run("plugin operation selection panicked".into()))??;
        let operation_cancellation = cancellation.clone();
        let execution = std::panic::AssertUnwindSafe(async move {
            operation.execute(service, operation_cancellation).await
        })
        .catch_unwind();
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(AgentError::Closed),
            result = execution => result.unwrap_or_else(|_| Err(AgentError::Run("plugin operation panicked".into()))),
        }
    }

    pub async fn pending_interactions(
        &self,
        session: &str,
    ) -> Result<Vec<InteractionRecord>, AgentError> {
        self.execute(PendingInteractions(session.into())).await
    }

    pub async fn interaction(
        &self,
        session: &str,
        interaction_id: RunId,
    ) -> Result<InteractionRecord, AgentError> {
        self.execute(GetInteraction {
            session_id: session.into(),
            interaction_id,
        })
        .await
    }
    pub async fn checkpoint(&self, run_id: RunId) -> Result<Checkpoint, AgentError> {
        self.execute(LoadCheckpoint(run_id)).await
    }
    pub async fn latest_checkpoint(&self, session: &str) -> Result<Option<Checkpoint>, AgentError> {
        self.execute(LatestCheckpoint(session.into())).await
    }

    /// Save an answer and return the request for the next execution. Saving and
    /// resuming remain separate so retries do not repeat business execution.
    pub async fn answer(
        &self,
        session: &str,
        interaction_id: RunId,
        value: serde_json::Value,
    ) -> Result<RunRequest, AgentError> {
        self.execute(AnswerInteraction {
            session_id: session.into(),
            interaction_id,
            value,
        })
        .await
    }

    pub async fn conversation(
        &self,
        key: &ConversationKey,
    ) -> Result<ConversationSnapshot, AgentError> {
        self.execute(ReadConversation(key.clone())).await
    }

    pub async fn append_conversation(
        &self,
        key: &ConversationKey,
        revision: u64,
        messages: Vec<Message>,
    ) -> Result<(), AgentError> {
        self.execute(AppendConversation {
            key: key.clone(),
            revision,
            messages,
        })
        .await
    }

    pub async fn memory(&self, namespace: &str, id: &str) -> Result<MemoryEntry, AgentError> {
        self.execute(GetMemory {
            namespace: namespace.into(),
            id: id.into(),
        })
        .await
    }
    pub async fn search_memory(
        &self,
        namespace: &str,
        query: MemoryQuery,
    ) -> Result<Vec<MemoryEntry>, AgentError> {
        self.execute(SearchMemory {
            namespace: namespace.into(),
            query,
        })
        .await
    }
    pub async fn save_memory(
        &self,
        namespace: &str,
        value: MemoryWrite,
        expected: Option<String>,
    ) -> Result<MemoryEntry, AgentError> {
        self.execute(SaveMemory {
            namespace: namespace.into(),
            value,
            expected,
        })
        .await
    }
    pub async fn delete_memory(
        &self,
        namespace: &str,
        id: &str,
        expected: &str,
    ) -> Result<(), AgentError> {
        self.execute(DeleteMemory {
            namespace: namespace.into(),
            id: id.into(),
            expected: expected.into(),
        })
        .await
    }

    /// Reserve the requested output before admitting a run. Missing plugins fail
    /// before model/tool execution, just like other invalid configuration.
    pub fn start_with_output<T: Send + 'static>(
        &self,
        request: RunRequest,
    ) -> Result<(RunHandle, T), AgentError> {
        if self.is_closed() {
            return Err(AgentError::Closed);
        }
        if !self
            .inner
            .outputs
            .iter()
            .any(|output| output.output_type() == std::any::TypeId::of::<T>())
        {
            return Err(AgentError::MissingService(std::any::type_name::<T>()));
        }
        let mut handle = self.start(request)?;
        let output = self.output(&mut handle)?;
        Ok((handle, output))
    }

    /// Take one configured output once. Preparation already happened in start;
    /// this only transfers ownership and cannot miss the first published event.
    pub fn output<T: Send + 'static>(&self, handle: &mut RunHandle) -> Result<T, AgentError> {
        self.check_owner(&handle.control)?;
        handle.outputs.take()
    }

    fn check_owner(&self, control: &RunControl) -> Result<(), AgentError> {
        if control.owner != self.inner.id {
            return Err(AgentError::InvalidConfiguration(
                "run belongs to a different Agent".into(),
            ));
        }
        Ok(())
    }

    /// Obtain a control token before moving the owning handle into Agent::wait.
    pub fn control(&self, handle: &RunHandle) -> Result<RunControl, AgentError> {
        self.check_owner(&handle.control)?;
        Ok(handle.control())
    }
    pub fn cancel(&self, run: &impl AsRef<RunControl>) -> Result<(), AgentError> {
        self.check_owner(run.as_ref())?;
        run.as_ref().cancel();
        Ok(())
    }
    pub fn steer(
        &self,
        run: &impl AsRef<RunControl>,
        message: impl Into<String>,
    ) -> Result<(), AgentError> {
        self.check_owner(run.as_ref())?;
        run.as_ref().steer(message)
    }
    pub fn revoke_tools(
        &self,
        run: &impl AsRef<RunControl>,
        ids: &[String],
    ) -> Result<(), AgentError> {
        self.check_owner(run.as_ref())?;
        run.as_ref().revoke_tools(ids)
    }
    pub fn budget_usage(&self, run: &impl AsRef<RunControl>) -> Result<Usage, AgentError> {
        self.check_owner(run.as_ref())?;
        Ok(run.as_ref().budget_usage())
    }
    /// The first receiver retains events published since start. Later receivers
    /// subscribe to new events only. Slow consumers can receive Lagged.
    pub fn events(
        &self,
        handle: &mut RunHandle,
    ) -> Result<broadcast::Receiver<EventEnvelope>, AgentError> {
        self.check_owner(&handle.control)?;
        Ok(handle.take_events())
    }
    /// Subscribe only to future events, without taking the initial receiver.
    pub fn subscribe(
        &self,
        handle: &RunHandle,
    ) -> Result<broadcast::Receiver<EventEnvelope>, AgentError> {
        self.check_owner(&handle.control)?;
        Ok(handle.events.subscribe())
    }
    /// Wait for the authoritative result. Dropping this future or its RunHandle
    /// cancels the execution, even while control tokens remain alive.
    pub async fn wait(&self, handle: RunHandle) -> Result<RunResult, AgentError> {
        self.check_owner(&handle.control)?;
        handle.wait().await
    }

    /// Configuration schemas and capabilities for host workflow editors.
    pub fn workflow_node_descriptors(&self) -> Vec<WorkflowNodeDescriptor> {
        self.inner.services.workflow_nodes.descriptors()
    }
    /// Sessions are exclusive within this instance. Different sessions can run
    /// concurrently; a second active run on the same session returns SessionBusy.
    pub fn start(&self, request: RunRequest) -> Result<RunHandle, AgentError> {
        self.start_inherited(request, None)
    }

    fn validate_request(&self, request: &RunRequest) -> Result<(), AgentError> {
        if request.resume.is_some() && !self.inner.agent_loop.supports_resume() {
            return Err(AgentError::InvalidConfiguration(
                "installed loop does not support durable resume".into(),
            ));
        }
        request.limits.validate()?;
        request.generation.validate()?;
        if request.session_id.trim().is_empty() {
            return Err(AgentError::InvalidConfiguration(
                "session ID must not be empty".into(),
            ));
        }
        Ok(())
    }

    fn start_inherited(
        &self,
        mut request: RunRequest,
        parent: Option<&RunContext>,
    ) -> Result<RunHandle, AgentError> {
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| AgentError::Run("Agent::start requires a Tokio runtime".into()))?;
        self.validate_request(&request)?;
        if self.is_closed() {
            return Err(AgentError::Closed);
        }
        let execution_id = RunId::new_v4();
        let mut outputs = RunOutputs::default();
        let mut extensions = request.extensions.clone();
        for factory in &self.inner.outputs {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                factory.prepare(&request, execution_id, &mut extensions, &mut outputs)
            }))
            .map_err(|_| {
                AgentError::InvalidConfiguration("output preparation panicked".into())
            })??;
            if !outputs.contains_type(factory.output_type()) {
                return Err(AgentError::InvalidConfiguration(
                    "output factory did not prepare its declared output type".into(),
                ));
            }
        }
        request.extensions = extensions;
        let cancellation = parent.map_or_else(CancellationToken::new, |p| p.children.child_token());
        {
            let mut runs = self.inner.runs.lock().unwrap_or_else(|e| e.into_inner());
            if runs.closed {
                return Err(AgentError::Closed);
            }
            if runs.active.contains_key(&request.session_id) {
                return Err(AgentError::SessionBusy(request.session_id));
            }
            runs.active
                .insert(request.session_id.clone(), cancellation.clone());
        }
        let run_id = request
            .resume
            .as_ref()
            .map_or_else(RunId::new_v4, |resume| resume.run_id);
        let (events, initial_events) = broadcast::channel(256);
        let steering = Arc::new(Mutex::new(crate::types::SteeringState {
            closed: !self.inner.agent_loop.supports_steering(),
            ..Default::default()
        }));
        let (result_sender, result) = oneshot::channel();
        let mut context = RunContext {
            model: request.model.clone(),
            node_execution: None,
            execution_counters: None,
            tool_dispatch_gate: Arc::default(),
            tool_slots: Arc::new(tokio::sync::Semaphore::new(
                request.limits.max_parallel_tools,
            )),
            revoked_tools: Arc::default(),
            execution_id,
            lineage: parent.map_or(
                RunLineage {
                    root_run_id: run_id,
                    parent_run_id: None,
                    depth: 0,
                },
                |p| RunLineage {
                    root_run_id: p.lineage.root_run_id,
                    parent_run_id: Some(p.run_id),
                    depth: p.lineage.depth + 1,
                },
            ),
            child_runs: parent.map_or_else(
                || Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                |p| p.child_runs.clone(),
            ),
            max_child_runs: parent.map_or(request.limits.max_child_runs, |p| {
                p.max_child_runs.min(request.limits.max_child_runs)
            }),
            max_child_depth: parent.map_or(request.limits.max_delegation_depth, |p| {
                p.max_child_depth.min(request.limits.max_delegation_depth)
            }),
            children: cancellation.child_token(),
            deadline: tokio::time::Instant::now() + request.limits.run_timeout,
            budget_journal: None,
            token_ceiling: request.limits.max_total_tokens,
            resources: Arc::new(Mutex::new(Vec::new())),
            usage: Arc::new(Mutex::new(crate::budget::UsageLedger::default())),
            publication: Arc::new(Mutex::new(())),
            sequence: Arc::new(AtomicU64::new(1)),
            observers: None,
            run_id,
            session_id: request.session_id.clone(),
            extensions: request.extensions.clone(),
            cancellation: cancellation.clone(),
            steering: steering.clone(),
            events: events.clone(),
            sinks: self.inner.sinks.clone(),
        };
        context.observers = Some(crate::observers::ObserverDispatcher::start(
            context.clone(),
            256,
        ));
        let usage = context.usage.clone();
        let revoked_tools = context.revoked_tools.clone();
        let inner = self.inner.clone();
        let completion = Arc::new(RunCompletion::default());
        let registration = RunRegistration {
            completion: completion.clone(),
            inner: inner.clone(),
            session_id: request.session_id.clone(),
        };
        let started = AgentEvent::RunStarted {
            run_id,
            session_id: request.session_id.clone(),
        };
        // Broadcast admission synchronously, even if cancellation arrives before
        // Tokio first polls the spawned task.
        context.publish(started);
        runtime.spawn(async move {
            crate::hooks::notify_started(
                &inner.services.lifecycle_hooks,
                &context,
                request.limits.cleanup_timeout,
            )
            .await;
            let result = RunNext {
                middleware: &inner.services.run_middleware,
                agent_loop: inner.agent_loop.as_ref(),
                services: &inner.services,
                request: &request,
                context: &context,
            }
            .run()
            .await;
            crate::hooks::notify_finished(
                &inner.services.lifecycle_hooks,
                &context,
                &result,
                request.limits.cleanup_timeout,
            )
            .await;
            context.seal_steering();
            if let Ok(result) = &result {
                context.emit_output(OutputEvent::RunResult(Box::new(result.clone())));
            }
            let terminal = match &result {
                Ok(result) => AgentEvent::RunFinished {
                    status: result.status.clone(),
                },
                Err(error) if matches!(error.primary(), AgentError::Cancelled) => {
                    AgentEvent::RunCancelled
                }
                Err(error) => AgentEvent::RunFailed(error.to_string()),
            };
            context.emit(terminal).await;
            if let Some(observers) = &context.observers {
                observers.finish(request.limits.cleanup_timeout).await;
            }
            drop(registration);
            let _ = result_sender.send(result);
        });
        Ok(RunHandle {
            completion,
            run_id,
            control: RunControl {
                owner: self.inner.id,
                run_id,
                revoked_tools,
                usage,
                cancellation,
                steering,
            },
            events,
            initial_events: Some(initial_events),
            result: Some(result),
            outputs,
        })
    }

    pub async fn run(&self, request: RunRequest) -> Result<RunResult, AgentError> {
        self.start(request)?.wait().await
    }

    /// Invoke this Agent as a child of an active run. Each child receives an
    /// isolated session/ledger with an allowance reserved from its parent before
    /// dispatch. Cancellation, deadline, root identity and delegation limits are
    /// inherited; host extensions are supplied explicitly by the caller.
    /// Rejected admission returns the local allowance and child slot. A confirmed
    /// budget reservation is corrected if dispatch fails; an uncertain storage
    /// write may retain a conservative durable reservation for recovery.
    /// Resume is a host orchestration operation and must be performed separately:
    /// a new delegation must not silently reparent an existing durable execution.
    pub async fn run_child(
        &self,
        mut request: RunRequest,
        parent: &RunContext,
    ) -> Result<RunResult, AgentError> {
        parent.check_cancelled()?;
        if request.resume.is_some() {
            return Err(AgentError::InvalidConfiguration(
                "child resume requires explicit host orchestration of the existing execution"
                    .into(),
            ));
        }
        self.validate_request(&request)?;
        if parent.lineage.depth >= parent.max_child_depth {
            return Err(AgentError::Run("delegation depth limit reached".into()));
        }
        let remaining = parent
            .deadline
            .saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(AgentError::Cancelled);
        }
        request.limits.run_timeout = request.limits.run_timeout.min(remaining);
        let (admission, allowance) =
            ChildAdmission::reserve(parent, request.limits.max_total_tokens)?;
        request.limits.max_total_tokens = allowance;
        // A failed/cancelled storage write may already have committed. Drop
        // refunds local admission, but do not guess a corrective durable write.
        parent.persist_budget().await?;
        let started = parent.check_cancelled().and_then(|()| {
            let remaining = parent
                .deadline
                .saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(AgentError::Cancelled);
            }
            request.limits.run_timeout = request.limits.run_timeout.min(remaining);
            self.start_inherited(request, Some(parent))
        });
        let handle = match started {
            Ok(handle) => handle,
            Err(error) => {
                drop(admission);
                return finish_with_cleanup(
                    Err(error),
                    parent.persist_budget().await.map_err(Into::into),
                );
            }
        };
        let mut permit = admission.dispatched();
        let usage = handle.control.usage.clone();
        let completion = handle.completion.clone();
        let cancel = handle.control.cancellation.clone();
        let fallback = cancel.clone();
        let mut scope = eliteforge_agent_plugin_kernel::ResourceScope::default();
        scope.on_cleanup(move || fallback.cancel());
        scope.on_shutdown(move || async move {
            cancel.cancel();
            completion.wait().await;
            Ok(())
        });
        parent.own_resources(scope);
        let result = handle.wait().await;
        let charged = usage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .including_reservations();
        let settlement = permit.settle_child(&charged, parent.token_ceiling);
        let result = finish_with_cleanup(result, settlement.map_err(Into::into));
        finish_with_cleanup(result, parent.persist_budget().await.map_err(Into::into))
    }

    /// Reject new runs, cancel existing runs, await their RAII cleanup, and close
    /// plugins. This is shared across every clone of the Agent handle.
    pub async fn close(&self) -> eliteforge_agent_plugin_kernel::ShutdownReport {
        self.close_with_timeout(std::time::Duration::from_secs(5))
            .await
    }

    /// Starts one owned shutdown task. Cancelling any caller does not abandon
    /// cleanup; all clones await and receive the same report. The first caller's
    /// timeout applies to each asynchronous plugin callback.
    pub async fn close_with_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> eliteforge_agent_plugin_kernel::ShutdownReport {
        {
            let mut runs = self.inner.runs.lock().unwrap_or_else(|e| e.into_inner());
            runs.closed = true;
            for token in runs.active.values() {
                token.cancel();
            }
            for token in runs.operations.values() {
                token.cancel();
            }
        }
        self.inner.services.operations.stop();
        let launch = {
            let mut state = self.inner.cleanup.lock().unwrap_or_else(|e| e.into_inner());
            if state.started {
                false
            } else {
                state.started = true;
                true
            }
        };
        if launch {
            let inner = self.inner.clone();
            tokio::spawn(async move {
                inner.services.operations.drain().await;
                loop {
                    let notified = inner.drained.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    let empty = {
                        let runs = inner.runs.lock().unwrap_or_else(|e| e.into_inner());
                        runs.active.is_empty() && runs.operations.is_empty()
                    };
                    if empty {
                        break;
                    }
                    notified.await;
                }
                let container = inner
                    .cleanup
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .container
                    .take();
                let report = std::panic::AssertUnwindSafe(async {
                    match container {
                        Some(mut container) => container.close_async(timeout).await,
                        None => eliteforge_agent_plugin_kernel::ShutdownReport::default(),
                    }
                })
                .catch_unwind()
                .await
                .unwrap_or_else(|_| {
                    eliteforge_agent_plugin_kernel::ShutdownReport {
                        panics: 1,
                        ..Default::default()
                    }
                });
                {
                    let mut state = inner.cleanup.lock().unwrap_or_else(|e| e.into_inner());
                    state.report = report;
                    state.complete = true;
                }
                inner.cleanup_done.notify_waiters();
            });
        }
        loop {
            let notified = self.inner.cleanup_done.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let state = self.inner.cleanup.lock().unwrap_or_else(|e| e.into_inner());
                if state.complete {
                    return state.report.clone();
                }
            }
            notified.await;
        }
    }

    pub fn is_closed(&self) -> bool {
        self.inner
            .runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .closed
    }
}

/// Before dispatch, dropping admission returns both resources. After dispatch,
/// the normal usage permit conservatively charges unknown provider outcomes.
struct ChildAdmission {
    child_runs: Arc<std::sync::atomic::AtomicUsize>,
    permit: Option<crate::budget::UsagePermit>,
    dispatched: bool,
}
impl ChildAdmission {
    fn reserve(parent: &RunContext, requested: u64) -> Result<(Self, u64), AgentError> {
        parent
            .child_runs
            .fetch_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |count| (count < parent.max_child_runs).then(|| count + 1),
            )
            .map_err(|_| AgentError::Run("child run count limit reached".into()))?;
        let mut admission = Self {
            child_runs: parent.child_runs.clone(),
            permit: None,
            dispatched: false,
        };
        let (permit, allowance) = crate::budget::UsagePermit::reserve_child(
            parent.usage.clone(),
            requested,
            parent.token_ceiling,
        )?;
        admission.permit = Some(permit);
        Ok((admission, allowance))
    }
    fn dispatched(mut self) -> crate::budget::UsagePermit {
        self.dispatched = true;
        self.permit.take().expect("child budget was reserved")
    }
}
impl Drop for ChildAdmission {
    fn drop(&mut self) {
        if !self.dispatched {
            if let Some(permit) = &mut self.permit {
                permit.release_unspent();
            }
            self.child_runs
                .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        }
    }
}

struct RunRegistration {
    completion: Arc<RunCompletion>,
    inner: Arc<AgentInner>,
    session_id: String,
}

struct OperationRegistration {
    inner: Arc<AgentInner>,
    id: RunId,
}
impl Drop for OperationRegistration {
    fn drop(&mut self) {
        if let Some(cancellation) = self
            .inner
            .runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .operations
            .remove(&self.id)
        {
            cancellation.cancel();
        }
        self.inner.drained.notify_waiters();
    }
}

impl Drop for RunRegistration {
    fn drop(&mut self) {
        self.inner
            .runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active
            .remove(&self.session_id);
        self.inner.drained.notify_waiters();
        self.completion
            .done
            .store(true, std::sync::atomic::Ordering::Release);
        self.completion.notify.notify_waiters();
    }
}

#[derive(Default)]
struct RunCompletion {
    done: std::sync::atomic::AtomicBool,
    notify: Notify,
}
impl RunCompletion {
    async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.done.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}

/// Cloneable controls for one execution, independent of its result receiver.
/// Dropping a control never cancels the run. The owning `RunHandle` (or its
/// `wait` future) must remain alive; dropping that owner still cancels the run.
#[derive(Clone)]
pub struct RunControl {
    owner: RunId,
    run_id: RunId,
    revoked_tools: Arc<Mutex<std::collections::BTreeSet<String>>>,
    usage: Arc<Mutex<crate::budget::UsageLedger>>,
    cancellation: CancellationToken,
    steering: Arc<Mutex<crate::types::SteeringState>>,
}

impl RunControl {
    pub fn run_id(&self) -> RunId {
        self.run_id
    }

    fn revoke_tools(&self, ids: &[String]) -> Result<(), AgentError> {
        crate::types::record_revocations(&self.revoked_tools, ids)
    }

    /// Conservative charged usage including in-flight reservations, not a bill.
    fn budget_usage(&self) -> Usage {
        self.usage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .including_reservations()
    }
    fn cancel(&self) {
        self.cancellation.cancel();
    }
    /// Queue text for the loop's next steering boundary. Success acknowledges
    /// in-memory admission, not persistence or model processing. Closed,
    /// cancelled and unsupported executions reject steering.
    fn steer(&self, message: impl Into<String>) -> Result<(), AgentError> {
        let mut state = self.steering.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed || self.cancellation.is_cancelled() {
            return Err(AgentError::Run(
                "run does not accept unaddressed live steering".into(),
            ));
        }
        state.pending.push_back(message.into());
        Ok(())
    }
}

/// Owns the result receiver and cancellation-on-drop lifetime of a run.
pub struct RunHandle {
    completion: Arc<RunCompletion>,
    pub run_id: RunId,
    control: RunControl,
    events: broadcast::Sender<EventEnvelope>,
    initial_events: Option<broadcast::Receiver<EventEnvelope>>,
    result: Option<oneshot::Receiver<Result<RunResult, AgentError>>>,
    outputs: RunOutputs,
}

impl AsRef<RunControl> for RunHandle {
    fn as_ref(&self) -> &RunControl {
        &self.control
    }
}
impl AsRef<RunControl> for RunControl {
    fn as_ref(&self) -> &RunControl {
        self
    }
}

impl RunHandle {
    fn control(&self) -> RunControl {
        self.control.clone()
    }
    fn take_events(&mut self) -> broadcast::Receiver<EventEnvelope> {
        self.initial_events
            .take()
            .unwrap_or_else(|| self.events.subscribe())
    }
    /// Consume the owner and wait for its final result. Obtain `control()`
    /// beforehand to steer or cancel from another task. Dropping this future
    /// cancels the run, even if control clones remain alive.
    async fn wait(mut self) -> Result<RunResult, AgentError> {
        self.result
            .take()
            .expect("run result receiver")
            .await
            .map_err(|_| AgentError::Run("run task ended without a result".into()))?
    }
}

impl Drop for RunHandle {
    fn drop(&mut self) {
        self.control.cancel();
    }
}
