//! Framework-owned plugin environment and immutable Agent capability bindings.
use crate::*;
use eliteforge_agent_plugin_core::{Plugin, PluginContainer, ServiceRegistry, ShutdownReport};
use futures::FutureExt;
use std::{
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// Selection of registered capabilities. Storage is always Framework-wide.
/// Provider IDs are the IDs of the plugins that registered each candidate.
#[derive(Clone, Default)]
pub struct AgentSelection {
    pub agent_loop: Option<String>,
    pub external_agent: Option<String>,
    pub model_strategy: Option<String>,
    pub model: Option<ModelRef>,
    pub context_assembler: Option<String>,
    pub context_budget: Option<String>,
    pub context_reducer: Option<String>,
    pub tool_providers: Option<Vec<String>>,
    /// Selected tool authority policies; empty for loops that never invoke tools.
    pub tool_policies: Option<Vec<String>>,
    /// Optional tool-ID restriction within the selected providers.
    pub tools: Option<Vec<String>>,
    pub skills: Option<Vec<String>>,
    /// Use a required, trusted SkillAccess supplied for every execution instead
    /// of the static list. The provider still authorizes every load/resource read.
    pub skills_from_host: bool,
    pub outputs: Option<Vec<String>>,
    pub context_sources: Option<Vec<String>>,
}
impl AgentSelection {
    pub(crate) fn validate_choices(&self, registry: &ServiceRegistry) -> Result<(), AgentError> {
        macro_rules! required {
            ($field:ident, $service:ty) => {
                if registry.contains::<$service>() && self.$field.is_none() {
                    return Err(AgentError::InvalidConfiguration(
                        concat!(
                            "select ",
                            stringify!($field),
                            " or configure a Framework default"
                        )
                        .into(),
                    ));
                }
            };
        }
        required!(agent_loop, LoopService);
        required!(external_agent, ExternalAgentService);
        required!(model_strategy, ModelStrategyService);
        required!(context_assembler, ContextAssemblerService);
        if self.context_assembler.is_none() {
            required!(context_budget, ContextBudgetPolicyService);
            required!(context_reducer, ContextReducerService);
        }
        Ok(())
    }
    pub(crate) fn bind(&self, registry: &ServiceRegistry) -> Result<ServiceRegistry, AgentError> {
        if let Some(names) = &self.skills {
            let mut seen = std::collections::HashSet::new();
            if names
                .iter()
                .any(|name| name.trim().is_empty() || !seen.insert(name))
            {
                return Err(AgentError::InvalidConfiguration(
                    "invalid or duplicate selected skill names".into(),
                ));
            }
        }
        let mut registry = registry.clone();
        registry.bind_one::<LoopService>(self.agent_loop.as_deref())?;
        registry.bind_one::<ExternalAgentService>(self.external_agent.as_deref())?;
        registry.bind_one::<ModelStrategyService>(self.model_strategy.as_deref())?;
        registry.bind_one::<ContextAssemblerService>(self.context_assembler.as_deref())?;
        if self.context_assembler.is_some()
            && registry.get::<ContextAssemblerService>().is_some()
            && self.context_budget.is_none()
            && self.context_reducer.is_none()
        {
            registry.omit::<ContextBudgetPolicyService>();
            registry.omit::<ContextReducerService>();
            registry.omit::<ContextLayoutService>();
        } else {
            registry.bind_one::<ContextBudgetPolicyService>(self.context_budget.as_deref())?;
            registry.bind_one::<ContextReducerService>(self.context_reducer.as_deref())?;
        }
        if let Some(ids) = &self.tool_providers {
            registry.bind_many::<ToolProviderService>(ids)?;
        }
        if let Some(ids) = &self.tool_policies {
            registry.bind_many::<ToolPolicyService>(ids)?;
        }
        if let Some(ids) = &self.outputs {
            registry.bind_many::<RunOutputService>(ids)?;
        }
        if let Some(ids) = &self.tools {
            let mut allowed = std::collections::HashSet::new();
            for id in ids {
                if id.trim().is_empty() || !allowed.insert(id.clone()) {
                    return Err(AgentError::InvalidConfiguration(format!(
                        "invalid or duplicate selected tool ID {id}"
                    )));
                }
            }
            let inventories: Option<Vec<_>> = registry
                .all::<ToolProviderService>()
                .iter()
                .map(|provider| provider.0.descriptors())
                .collect();
            if let Some(inventories) = inventories {
                let known: std::collections::HashSet<_> = inventories
                    .into_iter()
                    .flatten()
                    .map(|tool| tool.id)
                    .collect();
                if let Some(id) = allowed.iter().find(|id| !known.contains(*id)) {
                    return Err(AgentError::InvalidConfiguration(format!(
                        "selected tool {id} is not available from selected providers"
                    )));
                }
            }
            let allowed = Arc::new(allowed);
            registry.map_services::<ToolProviderService>(|service| {
                ToolProviderService(Arc::new(SelectedTools {
                    provider: service.0.clone(),
                    allowed: allowed.clone(),
                }))
            });
        }
        if let Some(ids) = &self.context_sources {
            registry.bind_many::<ContextContributorService>(ids)?;
        }
        Ok(registry)
    }
    fn with_defaults(mut self, defaults: &Self) -> Self {
        macro_rules! fill { ($($field:ident),* $(,)?) => { $(if self.$field.is_none() { self.$field = defaults.$field.clone(); })* }; }
        fill!(
            agent_loop,
            external_agent,
            model_strategy,
            model,
            context_assembler,
            context_budget,
            context_reducer,
            tool_providers,
            tools,
            skills,
            outputs,
            context_sources,
        );
        self
    }
}

/// Installs capability packs and plugins once for a Framework environment.
#[derive(Clone)]
pub struct FrameworkBuilder {
    assembly: crate::assembly::FrameworkAssembly,
    defaults: AgentSelection,
}
impl Default for FrameworkBuilder {
    fn default() -> Self {
        Self {
            assembly: crate::assembly::FrameworkAssembly::default(),
            defaults: AgentSelection {
                tool_providers: Some(Vec::new()),
                skills: Some(Vec::new()),
                context_sources: Some(Vec::new()),
                ..AgentSelection::default()
            },
        }
    }
}
impl FrameworkBuilder {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn plugin(mut self, plugin: impl Plugin + 'static) -> Self {
        self.assembly = self.assembly.plugin(plugin);
        self
    }
    pub fn plugin_arc(mut self, plugin: Arc<dyn Plugin>) -> Self {
        self.assembly = self.assembly.plugin_arc(plugin);
        self
    }
    pub fn plugin_factory(mut self, factory: ConfiguredPluginFactory) -> Self {
        self.assembly = self.assembly.plugin_factory(factory);
        self
    }
    pub fn capability_pack(mut self, pack: impl CapabilityPack + 'static) -> Self {
        self.assembly = self.assembly.capability_pack(pack);
        self
    }
    pub fn capability_pack_arc(mut self, pack: Arc<dyn CapabilityPack>) -> Self {
        self.assembly = self.assembly.capability_pack_arc(pack);
        self
    }
    pub fn capability_skills_root(mut self, root: impl Into<std::path::PathBuf>) -> Self {
        self.assembly = self.assembly.capability_skills_root(root);
        self
    }
    pub fn default_models(mut self, enabled: bool) -> Self {
        self.assembly = self.assembly.default_models(enabled);
        self
    }
    pub fn defaults(mut self, mut defaults: AgentSelection) -> Self {
        defaults.tool_providers.get_or_insert_with(Vec::new);
        defaults.skills.get_or_insert_with(Vec::new);
        defaults.context_sources.get_or_insert_with(Vec::new);
        self.defaults = defaults;
        self
    }
    pub async fn build(mut self) -> Result<Framework, AgentError> {
        self.assembly.prepare_framework()?;
        let packs = self.assembly.installed_packs()?;
        let mut container = self.assembly.install_plugins().await?;
        let registry = container.services().clone();
        if self.defaults.agent_loop.is_none()
            && registry.providers::<LoopService>() == ["builtin.framework-defaults"]
        {
            self.defaults.agent_loop = Some("builtin.framework-defaults".into());
        }
        // Only explicitly configured defaults are resolved here. Candidate sets
        // without defaults can still be selected by individual Agents.
        let validation =
            validate_registry(&registry).and_then(|_| validate_defaults(&self.defaults, &registry));
        if let Err(error) = validation {
            container.close_async(Duration::from_secs(5)).await;
            return Err(error);
        }
        Ok(Framework::from_container(
            container,
            registry,
            self.defaults,
            packs,
        ))
    }
}
struct SelectedTools {
    provider: Arc<dyn ToolProvider>,
    allowed: Arc<std::collections::HashSet<String>>,
}
#[async_trait::async_trait]
impl ToolProvider for SelectedTools {
    fn descriptors(&self) -> Option<Vec<ToolDescriptor>> {
        self.provider.descriptors().map(|mut tools| {
            tools.retain(|tool| self.allowed.contains(&tool.id));
            tools
        })
    }
    fn catalog_metadata(&self) -> Option<Vec<ToolCatalogEntry>> {
        self.provider.catalog_metadata().map(|mut tools| {
            tools.retain(|tool| self.allowed.contains(&tool.descriptor.id));
            tools
        })
    }
    async fn lease(&self, context: &RunContext) -> Result<ToolLease, AgentError> {
        let mut lease = self.provider.lease(context).await?;
        lease
            .tools
            .retain(|tool| self.allowed.contains(&tool.descriptor.id));
        Ok(lease)
    }
}
fn validate_registry(registry: &ServiceRegistry) -> Result<(), AgentError> {
    macro_rules! unique_ids { ($($service:ty),*) => { $(
        let mut providers = std::collections::HashSet::new();
        if registry.providers::<$service>().iter().any(|id| !providers.insert(*id)) {
            return Err(AgentError::InvalidConfiguration(format!("duplicate candidate IDs for {}", std::any::type_name::<$service>())));
        }
    )* }; }
    unique_ids!(
        LoopService,
        ExternalAgentService,
        ModelStrategyService,
        ContextAssemblerService,
        ContextBudgetPolicyService,
        ContextReducerService
    );
    ModelRuntime::from_registry(registry)?;

    let mut ids = std::collections::HashSet::new();
    let mut names = std::collections::HashSet::new();
    for provider in registry.all::<ToolProviderService>() {
        for descriptor in provider.0.descriptors().unwrap_or_default() {
            if !ids.insert(descriptor.id.clone()) || !names.insert(descriptor.call_name.clone()) {
                return Err(AgentError::InvalidConfiguration(format!(
                    "conflicting tool ID or call name: {}",
                    descriptor.id
                )));
            }
        }
    }
    Ok(())
}
fn validate_defaults(
    defaults: &AgentSelection,
    registry: &ServiceRegistry,
) -> Result<(), AgentError> {
    let mut registry = registry.clone();
    macro_rules! validate {
        ($field:ident, $service:ty) => {
            if let Some(id) = &defaults.$field {
                registry.bind_one::<$service>(Some(id))?;
            }
        };
    }
    validate!(agent_loop, LoopService);
    validate!(external_agent, ExternalAgentService);
    validate!(model_strategy, ModelStrategyService);
    validate!(context_assembler, ContextAssemblerService);
    validate!(context_budget, ContextBudgetPolicyService);
    validate!(context_reducer, ContextReducerService);
    if let Some(ids) = &defaults.tool_providers {
        registry.bind_many::<ToolProviderService>(ids)?;
    }
    if let Some(ids) = &defaults.outputs {
        registry.bind_many::<RunOutputService>(ids)?;
    }
    if let Some(ids) = &defaults.context_sources {
        registry.bind_many::<ContextContributorService>(ids)?;
    }
    Ok(())
}
struct FrameworkState {
    container: Option<PluginContainer>,
    agents: Vec<Weak<crate::agent::AgentInner>>,
    operations: usize,
    closing: bool,
    report: Option<ShutdownReport>,
}
struct FrameworkInner {
    registry: ServiceRegistry,
    defaults: AgentSelection,
    packs: Vec<Arc<dyn CapabilityPack>>,
    closed: AtomicBool,
    state: Mutex<FrameworkState>,
    drained: Notify,
    done: Notify,
    cancellation: CancellationToken,
    active_goals: Mutex<std::collections::HashSet<String>>,
}
/// Owns installed plugins and shared storage. Closing an Agent leaves these
/// resources alive; closing the Framework drains every Agent before plugins.
#[derive(Clone)]
pub struct Framework(Arc<FrameworkInner>);
pub(crate) struct GoalAdmission {
    framework: Framework,
    id: String,
}
impl Drop for GoalAdmission {
    fn drop(&mut self) {
        self.framework
            .0
            .active_goals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
    }
}
pub(crate) struct EnvironmentOperation {
    inner: Arc<FrameworkInner>,
    cancellation: CancellationToken,
}
impl Drop for EnvironmentOperation {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        self.cancellation.cancel();
        state.operations -= 1;
        self.inner.drained.notify_waiters();
    }
}
impl Framework {
    pub(crate) fn from_container(
        container: PluginContainer,
        registry: ServiceRegistry,
        defaults: AgentSelection,
        packs: Vec<Arc<dyn CapabilityPack>>,
    ) -> Self {
        Self(Arc::new(FrameworkInner {
            registry,
            defaults,
            packs,
            closed: AtomicBool::new(false),
            state: Mutex::new(FrameworkState {
                container: Some(container),
                agents: Vec::new(),
                operations: 0,
                closing: false,
                report: None,
            }),
            drained: Notify::new(),
            done: Notify::new(),
            cancellation: CancellationToken::new(),
            active_goals: Mutex::default(),
        }))
    }
    pub(crate) fn capability_packs(&self) -> Vec<Arc<dyn CapabilityPack>> {
        self.0.packs.clone()
    }
    pub fn builder() -> FrameworkBuilder {
        FrameworkBuilder::new()
    }
    pub fn agent(&self, selection: AgentSelection) -> AgentBuilder {
        AgentBuilder::in_framework(self.clone(), selection.with_defaults(&self.0.defaults))
    }
    pub(crate) fn same_environment(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    /// Read-only snapshot of installed services and provider identifiers.
    pub fn services(&self) -> Result<ServiceRegistry, AgentError> {
        self.registry()
    }
    pub(crate) fn registry(&self) -> Result<ServiceRegistry, AgentError> {
        if self.is_closed() {
            Err(AgentError::Closed)
        } else {
            Ok(self.0.registry.clone())
        }
    }
    pub(crate) fn admission(&self) -> Result<EnvironmentOperation, AgentError> {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if self.is_closed() {
            return Err(AgentError::Closed);
        }
        state.operations += 1;
        Ok(EnvironmentOperation {
            inner: self.0.clone(),
            cancellation: self.0.cancellation.child_token(),
        })
    }
    pub(crate) fn register_generation(
        &self,
        generation: &Arc<crate::agent::AgentInner>,
    ) -> Result<(), AgentError> {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if self.is_closed() {
            return Err(AgentError::Closed);
        }
        state.agents.retain(|agent| agent.strong_count() > 0);
        state.agents.push(Arc::downgrade(generation));
        Ok(())
    }
    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.0.cancellation.child_token()
    }
    pub(crate) async fn build_agent(&self, builder: AgentBuilder) -> Result<Agent, AgentError> {
        let admission = self.admission()?;
        let agent = tokio::select! {
            biased;
            _ = admission.cancellation.cancelled() => return Err(AgentError::Closed),
            agent = builder.assemble_shared() => agent?,
        };
        if self.is_closed() {
            agent.close().await;
            return Err(AgentError::Closed);
        }
        Ok(agent)
    }
    /// Framework administration does not require choosing or keeping an Agent.
    pub async fn execute<O: AgentOperation>(&self, operation: O) -> Result<O::Output, AgentError> {
        let admission = self.admission()?;
        let services = self
            .0
            .registry
            .get::<O::Service>()
            .map(|service| vec![service])
            .unwrap_or_else(|| self.0.registry.all::<O::Service>());
        let service =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| operation.select(services)))
                .map_err(|_| AgentError::Run("plugin operation selection panicked".into()))??;
        let execution = std::panic::AssertUnwindSafe(
            operation.execute(service, admission.cancellation.clone()),
        )
        .catch_unwind();
        tokio::select! { biased; _ = admission.cancellation.cancelled() => Err(AgentError::Closed), result = execution => result.unwrap_or_else(|_| Err(AgentError::Run("plugin operation panicked".into()))) }
    }
    pub fn sessions(&self) -> Result<SessionService, AgentError> {
        let registry = self.registry()?;
        Ok(SessionService {
            store: registry
                .get::<SessionLifecycleStoreService>()
                .ok_or(AgentError::MissingService("SessionLifecycleStore"))?
                .0
                .clone(),
            deletion: registry
                .get::<SessionStoreService>()
                .ok_or(AgentError::MissingService("SessionStore"))?
                .0
                .clone(),
        })
    }
    pub fn history(&self) -> Result<HistoryService, AgentError> {
        Ok(HistoryService(
            self.registry()?
                .get::<HistoryStoreService>()
                .ok_or(AgentError::MissingService("HistoryStore"))?
                .0
                .clone(),
        ))
    }
    pub fn goals(&self) -> Result<GoalService, AgentError> {
        Ok(GoalService(self.sessions()?.store))
    }
    pub(crate) fn admit_goal(&self, id: &str) -> Result<GoalAdmission, AgentError> {
        if self.is_closed() {
            return Err(AgentError::Closed);
        }
        if !self
            .0
            .active_goals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_owned())
        {
            return Err(AgentError::Run(
                "goal already has an active coordinator".into(),
            ));
        }
        Ok(GoalAdmission {
            framework: self.clone(),
            id: id.into(),
        })
    }
    pub fn runs(&self) -> Result<RunQueryService, AgentError> {
        Ok(RunQueryService(self.sessions()?.store))
    }
    pub fn events(&self) -> Result<EventQueryService, AgentError> {
        Ok(EventQueryService(self.sessions()?.store))
    }
    /// Framework and host repository writes automatically join this transaction.
    pub async fn transaction<T: Send>(
        &self,
        operation: impl std::future::Future<Output = Result<T, StateError>> + Send,
    ) -> Result<T, AgentError> {
        let admission = self.admission()?;
        let manager = self
            .registry()?
            .get::<TransactionManagerService>()
            .ok_or(AgentError::MissingService("TransactionManager"))?;
        let mut output = None;
        let work = Box::pin(async {
            output = Some(operation.await?);
            Ok(())
        });
        tokio::select! {
            biased;
            _ = admission.cancellation.cancelled() => return Err(AgentError::Closed),
            result = manager.0.transaction(work) => result?,
        }
        output.ok_or_else(|| AgentError::Run("transaction returned without a value".into()))
    }
    pub fn is_closed(&self) -> bool {
        self.0.closed.load(Ordering::Acquire)
    }
    pub async fn close(&self) -> ShutdownReport {
        self.close_with_timeout(Duration::from_secs(5)).await
    }
    pub fn close_with_timeout(
        &self,
        timeout: Duration,
    ) -> futures::future::BoxFuture<'_, ShutdownReport> {
        Box::pin(async move {
            let launch = {
                let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
                self.0.closed.store(true, Ordering::Release);
                self.0.cancellation.cancel();
                if state.closing {
                    false
                } else {
                    state.closing = true;
                    true
                }
            };
            if launch {
                let framework = self.clone();
                tokio::spawn(async move {
                    let agents: Vec<_> = framework
                        .0
                        .state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .agents
                        .iter()
                        .filter_map(Weak::upgrade)
                        .map(crate::agent::AgentGeneration::from_inner)
                        .collect();
                    let mut report = ShutdownReport::default();
                    for closed in futures::future::join_all(
                        agents.iter().map(|agent| agent.close_with_timeout(timeout)),
                    )
                    .await
                    {
                        report.merge(closed);
                    }
                    loop {
                        let notified = framework.0.drained.notified();
                        tokio::pin!(notified);
                        notified.as_mut().enable();
                        if framework
                            .0
                            .state
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .operations
                            == 0
                        {
                            break;
                        }
                        notified.await;
                    }
                    let container = framework
                        .0
                        .state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .container
                        .take();
                    if let Some(mut container) = container {
                        report.merge(container.close_async(timeout).await);
                    }
                    framework
                        .0
                        .state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .report = Some(report);
                    framework.0.done.notify_waiters();
                });
            }
            loop {
                let notified = self.0.done.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if let Some(report) = &self
                    .0
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .report
                {
                    return report.clone();
                }
                notified.await;
            }
        })
    }
}
