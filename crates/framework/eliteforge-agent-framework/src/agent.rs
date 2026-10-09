use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64},
    },
};

use futures::FutureExt;
use tokio::sync::{Mutex as AsyncMutex, Notify, broadcast, oneshot};
use tokio_util::sync::CancellationToken;

use crate::*;

/// Selects capabilities from an already installed Framework environment.
#[derive(Clone)]
pub struct AgentBuilder {
    framework: Framework,
    selection: AgentSelection,
}
impl AgentBuilder {
    pub(crate) fn in_framework(framework: Framework, selection: AgentSelection) -> Self {
        Self {
            framework,
            selection,
        }
    }
    pub async fn build(self) -> Result<Agent, AgentError> {
        self.framework.clone().build_agent(self).await
    }
    pub(crate) async fn assemble_shared(self) -> Result<Agent, AgentError> {
        let blueprint = self.clone();
        let generation = self.build_generation().await?;
        Ok(Agent::from_generation(generation, blueprint))
    }
    async fn build_generation(self) -> Result<AgentGeneration, AgentError> {
        let registry = self.framework.registry()?;
        self.selection.validate_choices(&registry)?;
        let registry = self.selection.bind(&registry)?;
        let (agent_loop, mut services, sinks, outputs) = (|| -> Result<_, AgentError> {
            let registry = &registry;
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
            let model_runtime = ModelRuntime::from_registry(registry)?;
            if let Some(model) = &self.selection.model {
                require_model(&model_runtime.snapshot().chat, model)?;
            }
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
            if registry
                .all::<ContextContributorService>()
                .iter()
                .any(|source| {
                    source.0.kind() == ContextSourceKind::History
                        || [INSTRUCTIONS_BLOCK, HISTORY_BLOCK].contains(&source.0.block_id())
                })
            {
                return Err(AgentError::InvalidConfiguration(
                    "builtin context sources are owned by the SDK".into(),
                ));
            }
            if registry.get::<ContextAssemblerService>().is_some()
                && (registry.get::<ContextLayoutService>().is_some()
                    || registry.get::<ContextBudgetPolicyService>().is_some()
                    || registry.get::<ContextReducerService>().is_some()
                    || registry
                        .all::<ContextContributorService>()
                        .iter()
                        .any(|s| s.0.kind() == ContextSourceKind::Data))
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
            let models: Option<Arc<dyn ChatExecutor>> = if connections.is_empty()
                && registry.get::<ModelRuntimeService>().is_none()
            {
                if strategy.is_some() {
                    return Err(AgentError::MissingService("ModelConnection"));
                }
                None
            } else {
                Some(Arc::new(
                    ModelInvoker::new((*model_runtime.snapshot().chat).clone(), strategy)?
                        .with_runtime(model_runtime.clone())
                        .with_recovery(
                            registry
                                .get::<ModelRecoveryPolicy>()
                                .map_or_else(ModelRecoveryPolicy::default, |policy| {
                                    (*policy).clone()
                                }),
                            registry
                                .all::<ModelCatalogRefreshService>()
                                .iter()
                                .map(|service| service.0.clone())
                                .collect(),
                        )
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
                                        .filter(|s| s.0.kind() == ContextSourceKind::Data)
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
                external_agent: registry
                    .get::<ExternalAgentService>()
                    .map(|service| service.0.clone()),
                extension_router: Some(RemoteExtensionDirectory::new(
                    registry
                        .all::<RemoteExtensionService>()
                        .iter()
                        .map(|s| s.0.clone()),
                )),
                operations: Arc::new(crate::capability_execution::ModelClient::from_runtime(
                    model_runtime.clone(),
                )),
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
                    .map(|service| service.0.clone())
                    .or_else(|| {
                        (!agent_loop.requires_conversations()).then(|| {
                            Arc::new(UnconfiguredConversationStore) as Arc<dyn ConversationStore>
                        })
                    })
                    .ok_or(AgentError::MissingService("ConversationStore"))?,
                tools: registry
                    .all::<ToolProviderService>()
                    .iter()
                    .map(|s| s.0.clone())
                    .collect(),
                context_sources: registry
                    .all::<ContextContributorService>()
                    .iter()
                    .filter(|s| s.0.kind() == ContextSourceKind::Instruction)
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
                if !types.insert(output.output_type()) && !output.aggregates() {
                    return Err(AgentError::InvalidConfiguration(
                        "multiple plugins provide the same run output type".into(),
                    ));
                }
            }
            Ok((agent_loop, services, sinks, outputs))
        })()?;
        let packs = self.framework.capability_packs();
        let versions: Vec<_> = packs
            .iter()
            .flat_map(|pack| pack.version_bindings())
            .collect();
        if !versions.is_empty()
            && let Some(store) = services.state.take()
        {
            services.state = Some(Arc::new(crate::capability_state::CapabilityBoundState {
                store,
                packs,
                versions,
            }));
        }
        let validation = async {
            agent_loop.validate_services_async(&services).await?;
            if let Some(models) = &services.models {
                models.validate_async().await?;
            }
            Ok::<_, AgentError>(())
        }
        .await;
        validation?;
        let operations = services.operations.clone();
        let framework = self.framework;
        let inner = Arc::new(AgentInner {
            id: RunId::new_v4(),
            cleanup: Mutex::new(CleanupState {
                started: false,
                complete: false,
                report: eliteforge_agent_plugin_core::ShutdownReport::default(),
            }),
            framework,
            default_model: self.selection.model.clone(),
            skill_access: self.selection.skills.as_ref().map(|names| SkillAccess {
                names: names.iter().cloned().collect(),
            }),
            registry,
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
            threads: Arc::new(ThreadRegistry::default()),
        });
        inner.framework.register_generation(&inner)?;
        let collaboration = Arc::new(CollaborationRuntime {
            agent: Arc::downgrade(&inner),
            threads: inner.threads.clone(),
            operations: operations.clone(),
        });
        Ok(AgentGeneration {
            operations,
            inner,
            collaboration,
        })
    }
}

struct RunRegistry {
    closed: bool,
    active: HashMap<String, CancellationToken>,
    operations: HashMap<RunId, CancellationToken>,
}

struct CleanupState {
    started: bool,
    complete: bool,
    report: eliteforge_agent_plugin_core::ShutdownReport,
}

pub(crate) struct AgentInner {
    id: RunId,
    framework: Framework,
    skill_access: Option<SkillAccess>,
    default_model: Option<ModelRef>,
    registry: eliteforge_agent_plugin_core::ServiceRegistry,
    cleanup: Mutex<CleanupState>,
    agent_loop: Arc<dyn AgentLoop>,
    services: AgentServices,
    sinks: Vec<Arc<dyn EventSink>>,
    outputs: Vec<Arc<dyn RunOutputFactory>>,
    runs: Mutex<RunRegistry>,
    drained: Notify,
    cleanup_done: Notify,
    threads: Arc<ThreadRegistry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssemblyOccupancy {
    pub kind: String,
    pub label: String,
}
#[derive(Debug, thiserror::Error)]
pub enum ReconfigureError {
    #[error("Agent is busy: {0:?}")]
    Busy(Vec<AssemblyOccupancy>),
    #[error("invalid reconfiguration: {0}")]
    Invalid(String),
    #[error("candidate failed; previous selection retained: {0}")]
    Rejected(String),
}
struct AssemblyState {
    generation: AgentGeneration,
    blueprint: AgentBuilder,
    updating: bool,
    closed: bool,
    next: u64,
    occupancy: BTreeMap<u64, AssemblyOccupancy>,
}
struct AgentAssembly {
    state: Mutex<AssemblyState>,
    transition: AsyncMutex<()>,
}
/// Stable Agent identity; all clones observe the same published assembly.
#[derive(Clone)]
pub struct Agent {
    assembly: Arc<AgentAssembly>,
    options: Option<ModelInvocationOptions>,
}
pub(crate) struct AdmissionLease {
    assembly: Weak<AgentAssembly>,
    id: u64,
    released: AtomicBool,
}
impl AdmissionLease {
    fn release(&self) {
        if !self
            .released
            .swap(true, std::sync::atomic::Ordering::AcqRel)
            && let Some(assembly) = self.assembly.upgrade()
        {
            assembly
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .occupancy
                .remove(&self.id);
        }
    }
}
impl Drop for AdmissionLease {
    fn drop(&mut self) {
        self.release();
    }
}
impl Agent {
    fn from_generation(generation: AgentGeneration, blueprint: AgentBuilder) -> Self {
        Self {
            assembly: Arc::new(AgentAssembly {
                state: Mutex::new(AssemblyState {
                    generation,
                    blueprint,
                    updating: false,
                    closed: false,
                    next: 0,
                    occupancy: BTreeMap::new(),
                }),
                transition: AsyncMutex::new(()),
            }),
            options: None,
        }
    }
    fn snapshot(&self) -> AgentGeneration {
        let generation = self
            .assembly
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .generation
            .clone();
        self.options.as_ref().map_or(generation.clone(), |options| {
            generation.with_model_options(options.clone())
        })
    }
    fn admit(
        &self,
        kind: &str,
        label: &str,
    ) -> Result<(AgentGeneration, Arc<AdmissionLease>), AgentError> {
        let mut state = self
            .assembly
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if state.closed || state.generation.is_closed() {
            return Err(AgentError::Closed);
        }
        if state.updating {
            return Err(AgentError::Run("Agent assembly update in progress".into()));
        }
        state.next += 1;
        let id = state.next;
        state.occupancy.insert(
            id,
            AssemblyOccupancy {
                kind: kind.into(),
                label: label.into(),
            },
        );
        let generation = self
            .options
            .as_ref()
            .map_or(state.generation.clone(), |options| {
                state.generation.with_model_options(options.clone())
            });
        Ok((
            generation,
            Arc::new(AdmissionLease {
                assembly: Arc::downgrade(&self.assembly),
                id,
                released: AtomicBool::new(false),
            }),
        ))
    }
    async fn reconfigure_selection(
        &self,
        framework: Framework,
        mut target: AgentBuilder,
    ) -> Result<(), ReconfigureError> {
        if !framework.same_environment(&target.framework) {
            return Err(ReconfigureError::Invalid(
                "cannot move Agent to another Framework".into(),
            ));
        }
        let environment = framework
            .admission()
            .map_err(|e| ReconfigureError::Invalid(e.to_string()))?;
        let cancellation = framework.cancellation_token();
        target.framework = framework;
        let transition = self.assembly.clone();
        let (sender, receiver) = oneshot::channel();
        {
            let mut state = transition.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.closed || state.generation.is_closed() {
                return Err(ReconfigureError::Invalid("Agent is closed".into()));
            }
            if state.updating {
                return Err(ReconfigureError::Busy(vec![AssemblyOccupancy {
                    kind: "update".into(),
                    label: "assembly update in progress".into(),
                }]));
            }
            let blockers =
                Self::blockers(&state).map_err(|e| ReconfigureError::Invalid(e.to_string()))?;
            if !blockers.is_empty() {
                return Err(ReconfigureError::Busy(blockers));
            }
            state.updating = true;
        }
        tokio::spawn(async move {
            let _environment = environment;
            let _transition = transition.transition.lock().await;
            let candidate = tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(AgentError::Closed),
                candidate = std::panic::AssertUnwindSafe(target.clone().build_generation()).catch_unwind() => {
                    candidate.unwrap_or_else(|_| Err(AgentError::Run("candidate selection panicked".into())))
                }
            };
            let result = match candidate {
                Ok(candidate) => {
                    let old = {
                        let mut state = transition.state.lock().unwrap_or_else(|e| e.into_inner());
                        state.blueprint = target;
                        std::mem::replace(&mut state.generation, candidate)
                    };
                    old.close().await;
                    Ok(())
                }
                Err(error) => Err(ReconfigureError::Rejected(error.to_string())),
            };
            transition
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .updating = false;
            let _ = sender.send(result);
        });
        receiver
            .await
            .unwrap_or_else(|_| Err(ReconfigureError::Invalid("selection update stopped".into())))
    }

    pub fn with_model_options(&self, options: ModelInvocationOptions) -> Self {
        Self {
            assembly: self.assembly.clone(),
            options: Some(options),
        }
    }
    pub fn start(&self, request: RunRequest) -> Result<RunHandle, AgentError> {
        let (generation, lease) = self.admit("task", &request.session_id)?;
        let mut handle = generation.start(request)?;
        handle.occupancy = Some(lease);
        Ok(handle)
    }
    pub fn start_with_output<T: Send + 'static>(
        &self,
        request: RunRequest,
    ) -> Result<(RunHandle, T), AgentError> {
        let (generation, lease) = self.admit("task", &request.session_id)?;
        let (mut handle, output) = generation.start_with_output(request)?;
        handle.occupancy = Some(lease);
        Ok((handle, output))
    }
    pub async fn connect_realtime(
        &self,
        request: RealtimeRequest,
    ) -> Result<RealtimeHandle, AgentError> {
        let (generation, lease) = self.admit("realtime", "realtime session")?;
        let mut handle = generation.connect_realtime(request).await?;
        handle.occupancy = Some(lease);
        Ok(handle)
    }
    pub async fn close_realtime(&self, handle: &RealtimeHandle) -> Result<(), AgentError> {
        // Closing releases an existing resource and remains valid after Agent shutdown.
        self.snapshot().close_realtime(handle).await?;
        if let Some(occupancy) = &handle.occupancy {
            occupancy.release();
        }
        Ok(())
    }
    pub fn reconfiguration_blockers(&self) -> Result<Vec<AssemblyOccupancy>, AgentError> {
        let state = self
            .assembly
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        Self::blockers(&state)
    }
    fn blockers(state: &AssemblyState) -> Result<Vec<AssemblyOccupancy>, AgentError> {
        let mut blockers: Vec<_> = state.occupancy.values().cloned().collect();
        let runs = state
            .generation
            .inner
            .runs
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for id in runs.active.keys() {
            blockers.push(AssemblyOccupancy {
                kind: "task".into(),
                label: id.to_string(),
            });
        }
        for id in runs.operations.keys() {
            blockers.push(AssemblyOccupancy {
                kind: "operation".into(),
                label: id.to_string(),
            });
        }
        drop(runs);
        let threads = state
            .generation
            .inner
            .threads
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for (id, thread) in &threads.records {
            let thread_state = thread.state.lock().unwrap_or_else(|e| e.into_inner());
            if thread_state
                .latest
                .as_ref()
                .is_some_and(|run| !run.done.load(std::sync::atomic::Ordering::Acquire))
            {
                blockers.push(AssemblyOccupancy {
                    kind: "thread".into(),
                    label: id.clone(),
                });
            }
        }
        Ok(blockers)
    }
    /// The Framework environment that owns this Agent's installed capabilities.
    pub fn framework(&self) -> Framework {
        self.assembly
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .blueprint
            .framework
            .clone()
    }
    pub(crate) fn goal_binding(
        &self,
        session: &str,
    ) -> Result<(AgentSelection, Arc<AdmissionLease>), AgentError> {
        let (_, lease) = self.admit("goal", session)?;
        let selection = self
            .assembly
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .blueprint
            .selection
            .clone();
        Ok((selection, lease))
    }
    /// Rebind an idle Agent to capabilities in the same Framework.
    /// Caller cancellation does not interrupt the owned update task.
    pub async fn reconfigure(&self, target: AgentBuilder) -> Result<(), ReconfigureError> {
        let framework = self
            .assembly
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .blueprint
            .framework
            .clone();
        self.reconfigure_selection(framework, target).await
    }
    pub async fn close(&self) -> eliteforge_agent_plugin_core::ShutdownReport {
        self.close_with_timeout(std::time::Duration::from_secs(5))
            .await
    }
    pub async fn close_with_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> eliteforge_agent_plugin_core::ShutdownReport {
        let _transition = self.assembly.transition.lock().await;
        let generation = {
            let mut state = self
                .assembly
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            state.closed = true;
            state.generation.clone()
        };
        generation.close_with_timeout(timeout).await
    }
    pub fn is_closed(&self) -> bool {
        let state = self
            .assembly
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        state.closed || (!state.updating && state.generation.is_closed())
    }
}
include!("agent_forward.rs");

#[derive(Clone)]
pub(crate) struct AgentGeneration {
    inner: Arc<AgentInner>,
    operations: Arc<dyn ModelOperations>,
    collaboration: Arc<CollaborationRuntime>,
}

/// Opaque realtime connection owned by the AgentGeneration's model runtime.
#[derive(Clone)]
pub struct RealtimeHandle {
    owner: RunId,
    session: Arc<dyn RealtimeSession>,
    occupancy: Option<Arc<AdmissionLease>>,
}

impl AgentGeneration {
    /// An AgentGeneration view with per-call model options; all views share lifecycle and services.
    pub(crate) fn from_inner(inner: Arc<AgentInner>) -> Self {
        let operations = inner.services.operations.clone();
        let collaboration = Arc::new(CollaborationRuntime {
            agent: Arc::downgrade(&inner),
            threads: inner.threads.clone(),
            operations: operations.clone(),
        });
        Self {
            inner,
            operations,
            collaboration,
        }
    }
    pub fn with_model_options(&self, options: ModelInvocationOptions) -> Self {
        let operations = self.operations.with_options(options);
        let collaboration = Arc::new(CollaborationRuntime {
            agent: Arc::downgrade(&self.inner),
            threads: self.inner.threads.clone(),
            operations: operations.clone(),
        });
        Self {
            inner: self.inner.clone(),
            operations,
            collaboration,
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
    pub async fn submit_image(
        &self,
        request: ImageGenerationRequest,
    ) -> Result<ImageJob, AgentError> {
        self.operations.submit_image(request).await
    }
    pub async fn query_image(&self, job: &ImageJob) -> Result<ImageJob, AgentError> {
        self.operations.query_image(job).await
    }
    pub async fn cancel_image(&self, job: &ImageJob) -> Result<(), AgentError> {
        self.operations.cancel_image(job).await
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
    pub async fn detect_content(
        &self,
        request: ContentDetectionRequest,
    ) -> Result<ContentDetectionResponse, AgentError> {
        self.operations.detect_content(request).await
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
            occupancy: None,
        })
    }
    fn check_realtime_owner(&self, handle: &RealtimeHandle) -> Result<(), AgentError> {
        if handle.owner != self.inner.id {
            return Err(AgentError::InvalidConfiguration(
                "realtime session belongs to a different AgentGeneration".into(),
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

    /// Dispatch an extension request using this AgentGeneration's installed service.
    pub async fn execute<O: AgentOperation>(&self, operation: O) -> Result<O::Output, AgentError> {
        let id = RunId::new_v4();
        let cancellation = CancellationToken::new();
        {
            let mut runs = self.inner.runs.lock().unwrap_or_else(|e| e.into_inner());
            if runs.closed || self.inner.framework.is_closed() {
                return Err(AgentError::Closed);
            }
            runs.operations.insert(id, cancellation.clone());
        }
        let _registration = OperationRegistration {
            inner: self.inner.clone(),
            id,
        };
        let services = self
            .inner
            .registry
            .get::<O::Service>()
            .map(|service| vec![service])
            .unwrap_or_else(|| self.inner.registry.all::<O::Service>());
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
                "run belongs to a different AgentGeneration".into(),
            ));
        }
        Ok(())
    }

    /// Obtain a control token before moving the owning handle into AgentGeneration::wait.
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
    /// Queue typed content without flattening media or host data into text.
    pub fn steer_content(
        &self,
        run: &impl AsRef<RunControl>,
        content: Content,
    ) -> Result<(), AgentError> {
        self.check_owner(run.as_ref())?;
        run.as_ref().steer_content(content)
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
    pub fn execution_snapshot(
        &self,
        run: &impl AsRef<RunControl>,
        result: &RunResult,
    ) -> Result<RunExecutionSnapshot, AgentError> {
        self.check_owner(run.as_ref())?;
        let control = run.as_ref();
        let restored = control.model_dispatches.restored();
        let limits = restored.as_ref().map_or_else(
            || control.limits.clone(),
            |previous| previous.limits.clone(),
        );
        let historical_calls_bound = restored
            .as_ref()
            .and_then(|previous| previous.call_bound)
            .or_else(|| {
                control
                    .initial_observation
                    .as_ref()
                    .and_then(|initial| initial.historical_calls_bound)
            });
        let incomplete = control.resumed
            || control
                .initial_observation
                .as_ref()
                .is_some_and(|initial| initial.incomplete);
        let overall = limits
            .max_model_calls_per_step
            .and_then(|per_step| limits.max_steps.checked_mul(per_step));
        let model_calls = control.continuation_budget.as_ref().map_or_else(
            || {
                if control.resumed {
                    historical_calls_bound
                        .and_then(|bound| bound.checked_add(control.model_dispatches.calls()))
                        .or(overall)
                        .map(|charged| overall.map_or(charged, |cap| charged.min(cap)))
                        .unwrap_or_else(|| control.model_dispatches.calls())
                } else {
                    control.model_dispatches.calls()
                }
            },
            |budget| budget.calls(),
        );
        let mut budget_usage = control.budget_usage();
        if let Some(previous) = &restored {
            budget_usage.input_tokens = budget_usage
                .input_tokens
                .max(previous.budget_usage.input_tokens);
            budget_usage.output_tokens = budget_usage
                .output_tokens
                .max(previous.budget_usage.output_tokens);
        }
        let original_timeout = limits.run_timeout.as_millis().min(u64::MAX as u128) as u64;
        let incoming_timeout = control.limits.run_timeout.as_millis().min(u64::MAX as u128) as u64;
        let deadline_unix_ms = control.deadline_unix_ms.min(
            control
                .deadline_unix_ms
                .saturating_sub(incoming_timeout)
                .saturating_add(original_timeout),
        );
        Ok(RunExecutionSnapshot {
            limits,
            budget_usage,
            model_calls,
            incomplete,
            historical_calls_bound,
            child_runs: control
                .child_runs
                .load(std::sync::atomic::Ordering::Acquire)
                .max(restored.as_ref().map_or(0, |previous| previous.child_runs)),
            steps: control
                .continuation_budget
                .as_ref()
                .map_or_else(
                    || {
                        control
                            .model_dispatches
                            .steps()
                            .saturating_add(restored.as_ref().map_or(0, |previous| previous.steps))
                    },
                    |budget| budget.steps(),
                )
                .max(result.steps),
            tool_calls: control
                .continuation_budget
                .as_ref()
                .map_or_else(
                    || {
                        control
                            .model_dispatches
                            .tools()
                            .saturating_add(restored.as_ref().map_or(0, |previous| previous.tools))
                    },
                    |budget| budget.tool_calls(),
                )
                .max(result.tool_calls),
            deadline_unix_ms,
            opaque: control.model_dispatches.opaque(),
        })
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
        if request.agent_code.is_some() {
            return Err(AgentError::InvalidConfiguration(
                "named Agent selection requires a host AgentCollaborationRuntime to resolve and authorize agentCode before execution".into(),
            ));
        }
        if request.resume.is_some() && !self.inner.agent_loop.supports_resume() {
            return Err(AgentError::InvalidConfiguration(
                "installed loop does not support durable resume".into(),
            ));
        }
        request.limits.validate()?;
        request.generation.validate()?;
        if request.scope().trim().is_empty()
            || request
                .conversation_branch
                .as_ref()
                .is_some_and(|b| b.trim().is_empty())
        {
            return Err(AgentError::InvalidConfiguration(
                "execution scope and conversation branch must not be empty".into(),
            ));
        }
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
        if request.model.is_none() {
            request.model = self.inner.default_model.clone();
        }
        if let Some(access) = &self.inner.skill_access {
            request.extensions.insert(access.clone());
        }
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| {
            AgentError::Run("AgentGeneration::start requires a Tokio runtime".into())
        })?;
        if let Some(parent) = parent
            && let Some(budget) = parent.extensions.get::<ContinuationBudget>()
        {
            budget.apply_limits(&mut request.limits);
            if let Some(cap) = parent.limits().max_model_calls_per_step {
                request.limits.max_model_calls_per_step = Some(
                    request
                        .limits
                        .max_model_calls_per_step
                        .map_or(cap, |local| local.min(cap)),
                );
            }
            request.extensions.insert((*budget).clone());
        }
        if let Some(journal) = parent.and_then(|parent| {
            parent
                .extensions
                .get::<crate::continuation::ContinuationJournalService>()
        }) {
            request.extensions.insert((*journal).clone());
        }
        let model_dispatches = parent
            .and_then(|parent| parent.extensions.get::<ModelDispatchCounter>())
            .or_else(|| request.extensions.get::<ModelDispatchCounter>())
            .map(|counter| (*counter).clone())
            .unwrap_or_default();
        request.extensions.insert(model_dispatches.clone());
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
        // Output preparation and host initialization cannot extend a root's
        // original absolute continuation deadline.
        let deadline = if let Some(input) = request.extensions.get::<ContinuationRequest>() {
            let remaining = input
                .initial_execution
                .deadline_unix_ms
                .saturating_sub(unix_time_ms());
            if remaining == 0 {
                return Err(AgentError::Cancelled);
            }
            request.limits.run_timeout = request
                .limits
                .run_timeout
                .min(std::time::Duration::from_millis(remaining));
            tokio::time::Instant::now() + request.limits.run_timeout
        } else {
            tokio::time::Instant::now() + request.limits.run_timeout
        };

        if request
            .extensions
            .get::<AgentCollaborationService>()
            .is_none()
        {
            request
                .extensions
                .insert(AgentCollaborationService(self.collaboration.clone()));
        }
        let run_id = request.resume.as_ref().map_or_else(
            || {
                request
                    .extensions
                    .get::<ChildRunIdentity>()
                    .map_or_else(|| request.run_id.unwrap_or_else(RunId::new_v4), |id| id.0)
            },
            |resume| resume.run_id,
        );
        if request.execution_scope.is_none()
            && (parent.is_some_and(|p| p.session_id == request.session_id)
                || request
                    .extensions
                    .get::<crate::goals::GoalRunParent>()
                    .is_some())
        {
            request.execution_scope = Some(format!("execution/{run_id}"));
        }
        if parent.is_some() && request.conversation_branch.is_none() {
            request.conversation_branch = Some(request.scope().to_owned());
        }
        let occupancy_key = request.occupancy_key();
        let cancellation = parent.map_or_else(CancellationToken::new, |p| p.children.child_token());
        {
            let mut runs = self.inner.runs.lock().unwrap_or_else(|e| e.into_inner());
            if runs.closed || self.inner.framework.is_closed() {
                return Err(AgentError::Closed);
            }
            if runs.active.contains_key(&occupancy_key) {
                return Err(AgentError::SessionBusy(request.session_id));
            }
            runs.active
                .insert(occupancy_key.clone(), cancellation.clone());
        }
        let (events, initial_events) = broadcast::channel(256);
        let steering = Arc::new(Mutex::new(crate::types::SteeringState {
            closed: !self.inner.agent_loop.supports_steering(),
            ..Default::default()
        }));
        let (result_sender, result) = oneshot::channel();
        let mut context = RunContext {
            model: request.model.clone(),
            limits: request.limits.clone(),
            node_execution: None,
            execution_counters: request
                .extensions
                .get::<ContinuationBudget>()
                .map(|budget| budget.execution_counters()),
            tool_dispatch_gate: Arc::default(),
            tool_slots: Arc::new(tokio::sync::Semaphore::new(
                request.limits.max_parallel_tools,
            )),
            revoked_tools: Arc::default(),
            execution_id,
            lineage: parent.map_or(
                RunLineage {
                    root_run_id: request
                        .extensions
                        .get::<crate::goals::GoalRunParent>()
                        .map_or(run_id, |p| p.root),
                    parent_run_id: request
                        .extensions
                        .get::<crate::goals::GoalRunParent>()
                        .map(|p| p.parent),
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
            deadline,
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
        context
            .extensions
            .insert(ScopedAgentServices(Arc::new(self.inner.services.clone())));
        context.observers = Some(crate::observers::ObserverDispatcher::start(
            context.clone(),
            256,
        ));
        let run_limits = request.limits.clone();
        let resumed = request.resume.is_some();
        let continuation_budget = request.extensions.get::<ContinuationBudget>();
        let initial_observation = request
            .extensions
            .get::<ContinuationRequest>()
            .map(|input| Arc::new(input.initial_execution.clone()));
        let child_runs = context.child_runs.clone();
        let deadline_unix_ms = unix_time_ms().saturating_add(
            context
                .deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .as_millis()
                .min(u64::MAX as u128) as u64,
        );
        let usage = context.usage.clone();
        let revoked_tools = context.revoked_tools.clone();
        let inner = self.inner.clone();
        let completion = Arc::new(RunCompletion::default());
        let registration = RunRegistration {
            completion: completion.clone(),
            inner: inner.clone(),
            session_id: occupancy_key,
        };
        let started = AgentEvent::RunStarted {
            run_id,
            session_id: request.session_id.clone(),
        };
        let lifecycle = self
            .inner
            .framework
            .services()?
            .get::<SessionLifecycleStoreService>();
        // Output reservations and the owning handle exist synchronously. SQL
        // admission still completes before any loop or lifecycle hook executes.
        context.publish(started);
        let kind = inner.agent_loop.execution_kind();
        let mut managed = ManagedRun {
            id: run_id,
            session_id: request.session_id.clone(),
            execution_scope: request.scope().to_owned(),
            conversation_branch: request
                .conversation_branch
                .clone()
                .unwrap_or_else(|| "main".into()),
            parent_run_id: context.lineage.parent_run_id,
            root_run_id: context.lineage.root_run_id,
            goal_id: request.goal_id.clone(),
            kind: kind.into(),
            status: ManagedRunStatus::Running,
            result: None,
            error: None,
            started_at_ms: unix_time_ms(),
            finished_at_ms: None,
            lease_until_ms: deadline_unix_ms
                .saturating_add(request.limits.cleanup_timeout.as_millis() as u64),
        };
        runtime.spawn(async move {
            let mut request = request;
            if let Some(lifecycle) = &lifecycle {
                if request.resume.is_some()
                    && request.execution_scope.is_none()
                    && let Ok(prior) = lifecycle.0.get_run(run_id).await
                {
                    request.execution_scope = Some(prior.execution_scope.clone());
                    request.conversation_branch = Some(prior.conversation_branch.clone());
                    managed.conversation_branch = prior.conversation_branch;
                    request.goal_id = prior.goal_id.clone();
                    managed.execution_scope = prior.execution_scope;
                    managed.parent_run_id = prior.parent_run_id;
                    managed.root_run_id = prior.root_run_id;
                    managed.goal_id = prior.goal_id;
                    context.lineage.root_run_id = managed.root_run_id;
                    context.lineage.parent_run_id = managed.parent_run_id;
                }
                if let Err(error) = lifecycle
                    .0
                    .register_run(managed.clone(), request.resume.is_some())
                    .await
                {
                    context.publish(AgentEvent::RunFailed(error.to_string()));
                    if let Some(observers) = &context.observers {
                        observers.finish(request.limits.cleanup_timeout).await;
                    }
                    drop(registration);
                    let _ = result_sender.send(Err(error.into()));
                    return;
                }
                context.extensions.insert(lifecycle.0.clone());
                context
                    .extensions
                    .insert(SessionPersistenceFailure::default());
            }
            crate::hooks::notify_started(
                &inner.services.lifecycle_hooks,
                &context,
                request.limits.cleanup_timeout,
            )
            .await;
            let mut result = RunNext {
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
            if let Some(failure) = context.extensions.get::<SessionPersistenceFailure>()
                && let Some(error) = failure.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
            {
                result = Err(error.into());
            }
            if let Some(lifecycle) = &lifecycle {
                managed.finished_at_ms = Some(unix_time_ms());
                managed.status = match &result {
                    Ok(value) => match value.status {
                        RunStatus::Completed => ManagedRunStatus::Completed,
                        RunStatus::AwaitingInput => ManagedRunStatus::AwaitingInput,
                        RunStatus::StepLimit | RunStatus::ToolCallLimit => {
                            ManagedRunStatus::Exhausted
                        }
                    },
                    Err(error) if matches!(error.primary(), AgentError::Cancelled) => {
                        ManagedRunStatus::Cancelled
                    }
                    Err(_) => ManagedRunStatus::Failed,
                };
                managed.result = result.as_ref().ok().cloned();
                managed.error = result.as_ref().err().map(ToString::to_string);
                if let Err(error) = lifecycle.0.finish_run(managed).await {
                    result = Err(error.into());
                }
            }
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
            context.publish(terminal);
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
                limits: run_limits,
                resumed,
                continuation_budget,
                initial_observation,
                child_runs,
                deadline_unix_ms,
                model_dispatches,
                cancellation,
                steering,
            },
            events,
            initial_events: Some(initial_events),
            result: Some(result),
            outputs,
            occupancy: None,
        })
    }

    pub async fn run(&self, request: RunRequest) -> Result<RunResult, AgentError> {
        self.start(request)?.wait().await
    }

    /// Invoke this AgentGeneration as a child of an active run. Each child receives an
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
        request: RunRequest,
        parent: &RunContext,
    ) -> Result<RunResult, AgentError> {
        Ok(self.run_child_with_id(request, parent).await?.result)
    }

    async fn run_child_with_id(
        &self,
        request: RunRequest,
        parent: &RunContext,
    ) -> Result<ChildRunResult, AgentError> {
        let handle = self.start_child_owned(request, parent).await?;
        let run_id = handle.run_id();
        let result = handle.wait().await?;
        Ok(ChildRunResult { run_id, result })
    }
    /// Native owned child admission. Obtain outputs/events/control before wait.
    /// Dropping either the handle or wait future cancels the child and retains
    /// its uncertain allowance until the parent's controlled cleanup completes.
    pub async fn start_child_owned(
        &self,
        mut request: RunRequest,
        parent: &RunContext,
    ) -> Result<ChildRunHandle, AgentError> {
        parent.check_cancelled()?;
        if request.resume.is_some() {
            return Err(AgentError::InvalidConfiguration(
                "child resume requires explicit reconciliation of the existing execution".into(),
            ));
        }
        if let Some(budget) = parent.extensions.get::<ContinuationBudget>() {
            budget.apply_limits(&mut request.limits);
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
        let child_session_id = request.session_id.clone();
        let child_depth = parent.lineage.depth.saturating_add(1);
        let (admission, allowance) =
            ChildAdmission::reserve(parent, request.limits.max_total_tokens)?;
        request.limits.max_total_tokens = allowance;
        parent.persist_budget().await?;
        let child_run_id = request.run_id.unwrap_or_else(RunId::new_v4);
        request.extensions.insert(ChildRunIdentity(child_run_id));
        if let Some(journal) = parent
            .extensions
            .get::<crate::continuation::ContinuationJournalService>()
        {
            journal.0.child_intent(parent, child_run_id).await?;
        }
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
        parent
            .emit(AgentEvent::Collaboration(Box::new(
                CollaborationEvent::ChildRunStarted {
                    child_run_id,
                    session_id: child_session_id,
                    depth: child_depth,
                },
            )))
            .await;
        let child_events = ChildEventGuard::new(parent.clone(), child_run_id);
        let permit = admission.dispatched();
        let completion = handle.completion.clone();
        let cancel = handle.control.cancellation.clone();
        let fallback = cancel.clone();
        let mut scope = eliteforge_agent_plugin_core::ResourceScope::default();
        scope.on_cleanup(move || fallback.cancel());
        scope.on_shutdown(move || async move {
            cancel.cancel();
            completion.wait().await;
            Ok(())
        });
        parent.own_resources(scope);
        Ok(ChildRunHandle {
            handle: Some(handle),
            parent: parent.clone(),
            permit: Some(permit),
            child_events,
        })
    }
    pub async fn start_child_with_output_owned<T: Send + 'static>(
        &self,
        request: RunRequest,
        parent: &RunContext,
    ) -> Result<(ChildRunHandle, T), AgentError> {
        if !self
            .inner
            .outputs
            .iter()
            .any(|output| output.output_type() == std::any::TypeId::of::<T>())
        {
            return Err(AgentError::MissingService(std::any::type_name::<T>()));
        }
        let mut child = self.start_child_owned(request, parent).await?;
        let output = self.output(child.handle.as_mut().expect("child handle"))?;
        Ok((child, output))
    }

    /// Reject new runs, cancel existing runs, await their RAII cleanup, and close
    /// plugins. This is shared across every clone of the AgentGeneration handle.
    pub async fn close(&self) -> eliteforge_agent_plugin_core::ShutdownReport {
        self.close_with_timeout(std::time::Duration::from_secs(5))
            .await
    }

    /// Starts one owned shutdown task. Cancelling any caller does not abandon
    /// cleanup; all clones await and receive the same report.
    /// Framework owns plugin shutdown; this only drains Agent work.
    pub async fn close_with_timeout(
        &self,
        _timeout: std::time::Duration,
    ) -> eliteforge_agent_plugin_core::ShutdownReport {
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
                let report = eliteforge_agent_plugin_core::ShutdownReport::default();
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
        self.inner.framework.is_closed()
            || self
                .inner
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

struct ChildEventGuard {
    parent: RunContext,
    child_run_id: RunId,
    finished: bool,
}

impl ChildEventGuard {
    fn new(parent: RunContext, child_run_id: RunId) -> Self {
        Self {
            parent,
            child_run_id,
            finished: false,
        }
    }

    fn finish(&mut self, event: CollaborationEvent) {
        self.parent
            .publish(AgentEvent::Collaboration(Box::new(event)));
        self.finished = true;
    }
}

impl Drop for ChildEventGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.parent.publish(AgentEvent::Collaboration(Box::new(
                CollaborationEvent::ChildRunFailed {
                    child_run_id: self.child_run_id,
                    cancelled: true,
                    message: "child run was interrupted before completion".into(),
                },
            )));
        }
    }
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
    limits: RunLimits,
    resumed: bool,
    continuation_budget: Option<Arc<ContinuationBudget>>,
    initial_observation: Option<Arc<RunExecutionSnapshot>>,
    child_runs: Arc<std::sync::atomic::AtomicUsize>,
    deadline_unix_ms: u64,
    model_dispatches: ModelDispatchCounter,
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
        self.steer_content(message.into().into())
    }
    fn steer_content(&self, content: Content) -> Result<(), AgentError> {
        let mut state = self.steering.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed || self.cancellation.is_cancelled() {
            return Err(AgentError::Run(
                "run does not accept unaddressed live steering".into(),
            ));
        }
        state.pending.push_back(content);
        Ok(())
    }
}

struct ChildRunIdentity(RunId);

/// An owned native child; all accounting and lifetime semantics match run_child.
pub struct ChildRunHandle {
    handle: Option<RunHandle>,
    parent: RunContext,
    permit: Option<crate::budget::UsagePermit>,
    child_events: ChildEventGuard,
}
impl ChildRunHandle {
    pub fn run_id(&self) -> RunId {
        self.child_events.child_run_id
    }
    pub fn control(&self) -> RunControl {
        self.handle.as_ref().expect("child handle").control()
    }
    pub fn events(&mut self) -> broadcast::Receiver<EventEnvelope> {
        self.handle.as_mut().expect("child handle").take_events()
    }
    pub async fn wait(mut self) -> Result<RunResult, AgentError> {
        let handle = self.handle.take().expect("child handle");
        let usage = handle.control.usage.clone();
        let result = handle.wait().await;
        let charged = usage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .including_reservations();
        let settlement = self
            .permit
            .as_mut()
            .expect("child permit")
            .settle_child(&charged, self.parent.token_ceiling);
        let result = finish_with_cleanup(result, settlement.map_err(Into::into));
        let result = finish_with_cleanup(
            result,
            self.parent.persist_budget().await.map_err(Into::into),
        );
        match &result {
            Ok(result) => self
                .child_events
                .finish(CollaborationEvent::ChildRunFinished {
                    child_run_id: self.run_id(),
                    status: result.status.clone(),
                    interactions: result.interactions.clone(),
                }),
            Err(error) => self
                .child_events
                .finish(CollaborationEvent::ChildRunFailed {
                    child_run_id: self.run_id(),
                    cancelled: matches!(error.primary(), AgentError::Cancelled),
                    message: error.to_string(),
                }),
        }
        result
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
    occupancy: Option<Arc<AdmissionLease>>,
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

const MAX_IN_PROCESS_THREADS: usize = 128;

#[derive(Default)]
struct ThreadRegistry {
    state: Mutex<ThreadRegistryState>,
}

#[derive(Default)]
struct ThreadRegistryState {
    records: BTreeMap<String, Arc<ThreadRecord>>,
    reservations: HashSet<String>,
}

impl ThreadRegistry {
    fn reserve(self: &Arc<Self>, thread_id: &str) -> Result<ThreadSlotReservation, AgentError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.records.len() + state.reservations.len() >= MAX_IN_PROCESS_THREADS {
            return Err(AgentError::Run(
                "in-process thread capacity exceeded".into(),
            ));
        }
        if state.records.contains_key(thread_id) || !state.reservations.insert(thread_id.to_owned())
        {
            return Err(AgentError::Run("thread ID collision".into()));
        }
        Ok(ThreadSlotReservation {
            registry: self.clone(),
            thread_id: thread_id.to_owned(),
            active: true,
        })
    }

    fn find(&self, thread_id: &str) -> Result<Arc<ThreadRecord>, AgentError> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .records
            .get(thread_id)
            .cloned()
            .ok_or_else(|| AgentError::Run("thread not found".into()))
    }
}

struct ThreadSlotReservation {
    registry: Arc<ThreadRegistry>,
    thread_id: String,
    active: bool,
}

impl ThreadSlotReservation {
    fn commit(mut self, record: Arc<ThreadRecord>) {
        let mut state = self
            .registry
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let reserved = state.reservations.remove(&self.thread_id);
        debug_assert!(reserved, "thread slot reservation must exist");
        let previous = state.records.insert(self.thread_id.clone(), record);
        debug_assert!(previous.is_none(), "reserved thread ID must be unique");
        self.active = false;
    }
}

impl Drop for ThreadSlotReservation {
    fn drop(&mut self) {
        if self.active {
            self.registry
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .reservations
                .remove(&self.thread_id);
        }
    }
}

struct ThreadRecord {
    session_id: String,
    transition: AsyncMutex<()>,
    state: Mutex<ThreadRecordState>,
}

struct ThreadRecordState {
    summary: ThreadSummary,
    latest: Option<Arc<ThreadRun>>,
}

struct ThreadRun {
    run_id: RunId,
    origin_execution_id: RunId,
    done: AtomicBool,
    terminal: Mutex<Option<ThreadStatus>>,
    result: Mutex<Option<Result<ThreadResult, String>>>,
    notify: Notify,
}

impl ThreadRun {
    fn new(run_id: RunId, origin_execution_id: RunId) -> Self {
        Self {
            run_id,
            origin_execution_id,
            done: AtomicBool::new(false),
            terminal: Mutex::new(None),
            result: Mutex::new(None),
            notify: Notify::new(),
        }
    }

    async fn wait(&self) -> Result<ThreadResult, AgentError> {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(result) = self
                .result
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            {
                return result.map_err(AgentError::Run);
            }
            notified.await;
        }
    }
}

struct CollaborationRuntime {
    agent: Weak<AgentInner>,
    threads: Arc<ThreadRegistry>,
    operations: Arc<dyn ModelOperations>,
}

impl CollaborationRuntime {
    fn agent(&self) -> Result<AgentGeneration, AgentError> {
        let inner = self.agent.upgrade().ok_or(AgentError::Closed)?;
        let collaboration = Arc::new(Self {
            agent: Arc::downgrade(&inner),
            threads: self.threads.clone(),
            operations: self.operations.clone(),
        });
        Ok(AgentGeneration {
            operations: self.operations.clone(),
            inner,
            collaboration,
        })
    }

    fn prepare_request(
        mut request: RunRequest,
        session_id: String,
        parent: &RunContext,
    ) -> RunRequest {
        request.session_id = session_id;
        request.extensions = parent.extensions.clone();
        if request.model.is_none() {
            request.model = parent.model.clone();
        }
        request.limits.max_steps = request.limits.max_steps.min(parent.limits.max_steps);
        request.limits.max_total_tokens = request.limits.max_total_tokens.min(parent.token_ceiling);
        request
    }

    fn launch(
        &self,
        agent: AgentGeneration,
        record: Arc<ThreadRecord>,
        run: Arc<ThreadRun>,
        handle: RunHandle,
        origin: RunContext,
    ) {
        tokio::spawn(async move {
            let result = agent.wait(handle).await;
            let (status, stored) = match result {
                Ok(result) => {
                    let status = ThreadStatus::from(&result.status);
                    (status, Ok(ThreadResult::from_run(run.run_id, &result)))
                }
                Err(error) => {
                    let status = if matches!(error.primary(), AgentError::Cancelled) {
                        ThreadStatus::Cancelled
                    } else {
                        ThreadStatus::Failed
                    };
                    (status, Err(error.to_string()))
                }
            };
            {
                let mut state = record.state.lock().unwrap_or_else(|e| e.into_inner());
                if state
                    .latest
                    .as_ref()
                    .is_some_and(|latest| latest.run_id == run.run_id)
                {
                    state.summary.status = status.clone();
                }
            }
            *run.terminal.lock().unwrap_or_else(|e| e.into_inner()) = Some(status.clone());
            origin
                .emit(AgentEvent::Collaboration(Box::new(
                    thread_run_finished_event(&record.session_id, run.run_id, status, &stored),
                )))
                .await;
            *run.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(stored);
            run.done.store(true, std::sync::atomic::Ordering::Release);
            run.notify.notify_waiters();
        });
    }

    async fn start_thread_run(
        &self,
        agent: AgentGeneration,
        record: Arc<ThreadRecord>,
        request: RunRequest,
        parent: &RunContext,
        cause: ThreadRunCause,
        interaction_id: Option<RunId>,
    ) -> Result<ThreadSummary, AgentError> {
        let handle = agent.start(request)?;
        let run = Arc::new(ThreadRun::new(handle.run_id, parent.execution_id()));
        let summary = {
            let mut state = record.state.lock().unwrap_or_else(|e| e.into_inner());
            state.latest = Some(run.clone());
            state.summary.status = ThreadStatus::Running;
            state.summary.latest_run_id = Some(handle.run_id);
            state.summary.clone()
        };
        parent
            .emit(AgentEvent::Collaboration(Box::new(
                CollaborationEvent::ThreadRunStarted {
                    thread_id: record.session_id.clone(),
                    run_id: handle.run_id,
                    cause,
                    interaction_id,
                },
            )))
            .await;
        self.launch(agent, record, run, handle, parent.clone());
        Ok(summary)
    }
}

fn thread_run_finished_event(
    thread_id: &str,
    run_id: RunId,
    status: ThreadStatus,
    result: &Result<ThreadResult, String>,
) -> CollaborationEvent {
    CollaborationEvent::ThreadRunFinished {
        thread_id: thread_id.to_owned(),
        run_id,
        status,
        interactions: result
            .as_ref()
            .map_or_else(|_| Vec::new(), |result| result.interactions.clone()),
        error: result.as_ref().err().cloned(),
    }
}

#[async_trait::async_trait]
impl AgentCollaborationRuntime for CollaborationRuntime {
    async fn spawn_child(
        &self,
        request: RunRequest,
        parent: &RunContext,
    ) -> Result<ChildRunResult, AgentError> {
        let agent = self.agent()?;
        let request =
            Self::prepare_request(request, format!("tool-child-{}", RunId::new_v4()), parent);
        agent.run_child_with_id(request, parent).await
    }

    async fn create_thread(
        &self,
        request: RunRequest,
        title: Option<String>,
        parent: &RunContext,
    ) -> Result<ThreadSummary, AgentError> {
        if title
            .as_deref()
            .is_some_and(|value| value.trim().is_empty() || value.len() > 200)
        {
            return Err(AgentError::InvalidConfiguration(
                "thread title must be nonempty and at most 200 bytes".into(),
            ));
        }
        let thread_id = format!("thread-{}", RunId::new_v4());
        let reservation = self.threads.reserve(&thread_id)?;
        let agent = self.agent()?;
        let request = Self::prepare_request(request, thread_id.clone(), parent);
        let handle = agent.start(request)?;
        let run = Arc::new(ThreadRun::new(handle.run_id, parent.execution_id()));
        let summary = ThreadSummary {
            thread_id: thread_id.clone(),
            title,
            status: ThreadStatus::Running,
            latest_run_id: Some(handle.run_id),
        };
        let record = Arc::new(ThreadRecord {
            session_id: thread_id.clone(),
            transition: AsyncMutex::new(()),
            state: Mutex::new(ThreadRecordState {
                summary: summary.clone(),
                latest: Some(run.clone()),
            }),
        });
        reservation.commit(record.clone());
        parent
            .emit(AgentEvent::Collaboration(Box::new(
                CollaborationEvent::ThreadCreated {
                    summary: summary.clone(),
                },
            )))
            .await;
        parent
            .emit(AgentEvent::Collaboration(Box::new(
                CollaborationEvent::ThreadRunStarted {
                    thread_id: thread_id.clone(),
                    run_id: handle.run_id,
                    cause: ThreadRunCause::Created,
                    interaction_id: None,
                },
            )))
            .await;
        self.launch(agent, record.clone(), run, handle, parent.clone());
        Ok(summary)
    }

    async fn send_thread(
        &self,
        thread_id: &str,
        request: RunRequest,
        parent: &RunContext,
    ) -> Result<ThreadSummary, AgentError> {
        let record = self.threads.find(thread_id)?;
        let _transition = record.transition.lock().await;
        {
            let state = record.state.lock().unwrap_or_else(|e| e.into_inner());
            if state
                .latest
                .as_ref()
                .is_some_and(|run| !run.done.load(std::sync::atomic::Ordering::Acquire))
            {
                return Err(AgentError::SessionBusy(thread_id.into()));
            }
            if state.summary.status == ThreadStatus::AwaitingInput {
                return Err(AgentError::Run(
                    "thread is awaiting input; use thread_answer".into(),
                ));
            }
        }
        let agent = self.agent()?;
        let request = Self::prepare_request(request, record.session_id.clone(), parent);
        self.start_thread_run(
            agent,
            record.clone(),
            request,
            parent,
            ThreadRunCause::Message,
            None,
        )
        .await
    }

    async fn answer_thread(
        &self,
        thread_id: &str,
        interaction_id: RunId,
        value: serde_json::Value,
        parent: &RunContext,
    ) -> Result<ThreadSummary, AgentError> {
        let record = self.threads.find(thread_id)?;
        let _transition = record.transition.lock().await;
        {
            let state = record.state.lock().unwrap_or_else(|e| e.into_inner());
            if state
                .latest
                .as_ref()
                .is_some_and(|run| !run.done.load(std::sync::atomic::Ordering::Acquire))
            {
                return Err(AgentError::SessionBusy(thread_id.into()));
            }
            if state.summary.status != ThreadStatus::AwaitingInput {
                return Err(AgentError::Run("thread is not awaiting input".into()));
            }
        }
        let agent = self.agent()?;
        let request = agent
            .answer(&record.session_id, interaction_id, value)
            .await?;
        let request = Self::prepare_request(request, record.session_id.clone(), parent);
        self.start_thread_run(
            agent,
            record.clone(),
            request,
            parent,
            ThreadRunCause::InteractionResponse,
            Some(interaction_id),
        )
        .await
    }

    async fn wait_thread(
        &self,
        thread_id: &str,
        run_id: Option<RunId>,
        parent: &RunContext,
    ) -> Result<ThreadResult, AgentError> {
        let record = self.threads.find(thread_id)?;
        let run = record
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .latest
            .clone()
            .ok_or_else(|| AgentError::Run("thread has no run".into()))?;
        if run_id.is_some_and(|id| id != run.run_id) {
            return Err(AgentError::Run(
                "requested run is not the latest thread run".into(),
            ));
        }
        let result = run.wait().await;
        if parent.execution_id() != run.origin_execution_id {
            let status = run
                .terminal
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .unwrap_or(ThreadStatus::Failed);
            let stored = match &result {
                Ok(result) => Ok(result.clone()),
                Err(error) => Err(error.to_string()),
            };
            parent
                .emit(AgentEvent::Collaboration(Box::new(
                    thread_run_finished_event(thread_id, run.run_id, status, &stored),
                )))
                .await;
        }
        result
    }

    async fn read_thread(&self, thread_id: &str, limit: usize) -> Result<ThreadView, AgentError> {
        let record = self.threads.find(thread_id)?;
        let agent = self.agent()?;
        let mut conversation = agent
            .conversation(&ConversationKey::new(record.session_id.clone()))
            .await?;
        if conversation.messages.len() > limit {
            let start = conversation.messages.len() - limit;
            conversation.messages = conversation.messages.split_off(start);
        }
        Ok(ThreadView {
            summary: record
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .summary
                .clone(),
            conversation,
        })
    }

    async fn list_threads(&self, _parent: &RunContext) -> Result<Vec<ThreadSummary>, AgentError> {
        Ok(self
            .threads
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .records
            .values()
            .map(|record| {
                record
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .summary
                    .clone()
            })
            .collect())
    }

    async fn close_thread(&self, thread_id: &str, _parent: &RunContext) -> Result<(), AgentError> {
        let record = self.threads.find(thread_id)?;
        let _transition = record.transition.lock().await;
        {
            let state = record.state.lock().unwrap_or_else(|e| e.into_inner());
            if state
                .latest
                .as_ref()
                .is_some_and(|run| !run.done.load(std::sync::atomic::Ordering::Acquire))
            {
                return Err(AgentError::SessionBusy(thread_id.into()));
            }
        }
        {
            let mut registry = self.threads.state.lock().unwrap_or_else(|e| e.into_inner());
            match registry.records.get(thread_id) {
                Some(current) if Arc::ptr_eq(current, &record) => {
                    registry.records.remove(thread_id);
                }
                _ => return Err(AgentError::Run("thread not found".into())),
            }
        }
        _parent
            .emit(AgentEvent::Collaboration(Box::new(
                CollaborationEvent::ThreadClosed {
                    thread_id: thread_id.to_owned(),
                },
            )))
            .await;
        Ok(())
    }
}
