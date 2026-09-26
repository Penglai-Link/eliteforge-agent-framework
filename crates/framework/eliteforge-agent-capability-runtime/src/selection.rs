use crate::*;
use async_trait::async_trait;
use eliteforge_agent_context_contracts::*;
use eliteforge_agent_core::*;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, OnceLock},
};

/// Immutable selection declaration. Each Agent build creates fresh runtime resources.
#[derive(Clone)]
pub struct ManagedCapabilityPack {
    manager: CapabilityPackManager,
    pack: InstalledCapabilityPack,
    scope: String,
    namespace: String,
    configuration_version: String,
    configuration: Value,
    selected: BTreeSet<String>,
    selected_plugins: BTreeSet<String>,
    selected_pages: BTreeSet<String>,
    selected_mcp: BTreeSet<String>,
    selected_skills: BTreeSet<String>,
    included: Vec<Arc<dyn CapabilityPack>>,
}
impl CapabilityPackManager {
    /// Select an exact installed version. Installing a newer version never changes this value.
    pub fn select(
        &self,
        package: &str,
        requirement: &semver::VersionReq,
        scope: impl Into<String>,
        configuration_version: impl Into<String>,
        configuration: Value,
    ) -> RuntimeResult<ManagedCapabilityPack> {
        self.select_configured(
            package,
            requirement,
            scope,
            BTreeMap::from([(
                package.into(),
                CapabilityPackageConfiguration {
                    version: configuration_version.into(),
                    value: configuration,
                    contributions: None,
                },
            )]),
        )
    }
    /// Configure each package independently; dependencies do not inherit the
    /// root package's configuration or implementation choices.
    pub fn select_configured(
        &self,
        package: &str,
        requirement: &semver::VersionReq,
        scope: impl Into<String>,
        configurations: BTreeMap<String, CapabilityPackageConfiguration>,
    ) -> RuntimeResult<ManagedCapabilityPack> {
        let packs = self.resolve(package, requirement)?;
        let scope = scope.into();
        if configurations
            .keys()
            .any(|id| !packs.iter().any(|pack| &pack.manifest.id == id))
        {
            return Err(RuntimeError::Invalid(
                "configuration references a package outside the dependency graph".into(),
            ));
        }
        let mut root = None;
        let mut included: Vec<Arc<dyn CapabilityPack>> = Vec::new();
        for pack in packs {
            let config = configurations
                .get(&pack.manifest.id)
                .cloned()
                .unwrap_or_default();
            let mut selection = ManagedCapabilityPack {
                manager: self.clone(),
                selected_plugins: pack.manifest.plugins.iter().cloned().collect(),
                selected_pages: pack
                    .manifest
                    .pages
                    .iter()
                    .map(|page| page.id.clone())
                    .collect(),
                selected_mcp: pack
                    .manifest
                    .mcp
                    .iter()
                    .map(|server| server.id.clone())
                    .collect(),
                selected_skills: pack.manifest.skills.iter().cloned().collect(),
                selected: pack
                    .manifest
                    .implementations
                    .iter()
                    .map(|i| i.id.clone())
                    .collect(),
                pack,
                scope: scope.clone(),
                namespace: scope.clone(),
                configuration_version: config.version,
                configuration: config.value,
                included: Vec::new(),
            };
            if let Some(contributions) = config.contributions {
                selection = selection.contributions(contributions)?;
            }
            if selection.pack.manifest.id == package {
                root = Some(selection);
            } else {
                included.push(Arc::new(selection));
            }
        }
        let mut root =
            root.ok_or_else(|| RuntimeError::Invalid("missing selected package".into()))?;
        root.included = included;
        Ok(root)
    }
}
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CapabilityPackageConfiguration {
    pub version: String,
    pub value: Value,
    pub contributions: Option<CapabilityContributionSelection>,
}
impl Default for CapabilityPackageConfiguration {
    fn default() -> Self {
        Self {
            version: "default".into(),
            value: Value::Null,
            contributions: None,
        }
    }
}
impl ManagedCapabilityPack {
    /// Optional lists select contributions independently; omitted lists keep the
    /// existing selection. Dependency implementations are added transitively.
    pub fn contributions(
        mut self,
        selection: CapabilityContributionSelection,
    ) -> RuntimeResult<Self> {
        fn choose(
            current: &mut BTreeSet<String>,
            ids: Option<Vec<String>>,
            available: BTreeSet<String>,
        ) -> RuntimeResult<()> {
            if let Some(ids) = ids {
                let ids: BTreeSet<_> = ids.into_iter().collect();
                if !ids.is_subset(&available) {
                    return Err(RuntimeError::Invalid(
                        "unknown contribution in selection".into(),
                    ));
                }
                *current = ids;
            }
            Ok(())
        }
        choose(
            &mut self.selected_plugins,
            selection.plugins,
            self.pack.manifest.plugins.iter().cloned().collect(),
        )?;
        choose(
            &mut self.selected_pages,
            selection.pages,
            self.pack
                .manifest
                .pages
                .iter()
                .map(|p| p.id.clone())
                .collect(),
        )?;
        choose(
            &mut self.selected_mcp,
            selection.mcp,
            self.pack
                .manifest
                .mcp
                .iter()
                .map(|p| p.id.clone())
                .collect(),
        )?;
        choose(
            &mut self.selected_skills,
            selection.skills,
            self.pack.manifest.skills.iter().cloned().collect(),
        )?;
        if let Some(ids) = selection.implementations {
            self = self.implementations(ids)?;
        }
        Ok(self)
    }
    pub fn implementations(
        mut self,
        selected: impl IntoIterator<Item = impl Into<String>>,
    ) -> RuntimeResult<Self> {
        let selected: BTreeSet<String> = selected.into_iter().map(Into::into).collect();
        if selected.iter().any(|id| {
            !self
                .pack
                .manifest
                .implementations
                .iter()
                .any(|i| &i.id == id)
        }) {
            return Err(RuntimeError::Invalid(
                "selection contains unknown implementation".into(),
            ));
        }
        let mut selected = selected;
        loop {
            let previous = selected.len();
            for declaration in &self.pack.manifest.implementations {
                if selected.contains(&declaration.id) {
                    for dependency in &declaration.dependencies {
                        if self
                            .pack
                            .manifest
                            .implementations
                            .iter()
                            .any(|i| &i.id == dependency)
                        {
                            selected.insert(dependency.clone());
                        }
                        if self.pack.manifest.plugins.contains(dependency) {
                            self.selected_plugins.insert(dependency.clone());
                        }
                        if self
                            .pack
                            .manifest
                            .mcp
                            .iter()
                            .any(|server| &server.id == dependency)
                        {
                            self.selected_mcp.insert(dependency.clone());
                        }
                    }
                }
            }
            if selected.len() == previous {
                break;
            }
        }
        self.selected = selected;
        Ok(self)
    }
    pub fn installed(&self) -> &InstalledCapabilityPack {
        &self.pack
    }
    pub fn scope(&self) -> &str {
        &self.scope
    }
}
impl CapabilityPack for ManagedCapabilityPack {
    fn requires_skills_runtime(&self) -> bool {
        self.pack
            .manifest
            .implementations
            .iter()
            .any(|i| self.selected.contains(&i.id) && i.extension == "framework.skills")
    }
    fn preflight(&self) -> Result<(), AgentError> {
        self.manager
            .validate_versions(&[PackVersionRequirement {
                package: self.pack.manifest.id.clone(),
                version: self.pack.manifest.version.clone(),
                digest: self.pack.digest.clone(),
            }])
            .map_err(|e| AgentError::InvalidConfiguration(e.to_string()))?;
        if !self.pack.manifest.config_schema.is_null() {
            let schema = jsonschema::options()
                .should_validate_formats(true)
                .build(&self.pack.manifest.config_schema)
                .map_err(|e| AgentError::InvalidConfiguration(e.to_string()))?;
            if !schema.is_valid(&self.configuration) {
                return Err(AgentError::InvalidConfiguration(format!(
                    "configuration does not match schema for {}",
                    self.id()
                )));
            }
        }
        Ok(())
    }
    fn id(&self) -> &str {
        &self.pack.manifest.id
    }
    fn configuration(&self) -> &CapabilityConfig {
        &self.configuration
    }
    fn rebuildable(&self) -> bool {
        true
    }
    fn for_agent_scope(&self, scope: &str) -> Option<Arc<dyn CapabilityPack>> {
        let mut selected = self.clone();
        selected.scope = format!("{}:{scope}", self.namespace);
        selected.included = selected
            .included
            .iter()
            .map(|pack| pack.for_agent_scope(scope).unwrap_or_else(|| pack.clone()))
            .collect();
        Some(Arc::new(selected))
    }
    fn included_packs(&self) -> Vec<Arc<dyn CapabilityPack>> {
        self.included.clone()
    }
    fn skill_sources(&self) -> Vec<CapabilitySkillSource> {
        self.pack
            .manifest
            .skills
            .iter()
            .filter(|source| self.selected_skills.contains(*source))
            .map(|relative| CapabilitySkillSource::directory(self.pack.path.join(relative)))
            .collect()
    }
    fn reconfiguration_blockers(&self) -> Result<Vec<String>, AgentError> {
        let mut blockers: Vec<_> = self
            .manager
            .usage
            .blockers(self.id())
            .into_iter()
            .filter(|b| {
                b.scope == self.scope && !matches!(b.kind.as_str(), "runtime" | "selection")
            })
            .map(|b| format!("{}: {}", b.kind, b.label))
            .collect();
        blockers.extend(
            self.manager
                .durable_pins()
                .map_err(|e| AgentError::Run(e.to_string()))?
                .into_iter()
                .filter(|pin| {
                    pin.scope == self.namespace
                        && pin.versions.iter().any(|v| v.package == self.id())
                })
                .map(|pin| format!("pending task: {}", pin.id)),
        );
        Ok(blockers)
    }
    fn lock_reconfiguration(&self) -> Result<Box<dyn Send>, AgentError> {
        let blockers = self.reconfiguration_blockers()?;
        if !blockers.is_empty() {
            return Err(AgentError::Run(format!("package occupied: {blockers:?}")));
        }
        let instances: Vec<_> = self
            .manager
            .instances
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter_map(|instance| instance.upgrade())
            .filter(|instance| {
                instance.identity.package == self.id()
                    && instance.identity.scope == self.scope
                    && !instance.is_retired()
            })
            .collect();
        for instance in &instances {
            instance.pause_admission();
        }
        let mut admission = PausedInstances(instances);
        let guard = self
            .manager
            .usage
            .begin_scope_update(self.id(), &self.scope)
            .map_err(|e| AgentError::Run(e.to_string()))?;
        let instances = std::mem::take(&mut admission.0);
        Ok(Box::new((guard, PausedInstances(instances))))
    }
    fn version_bindings(&self) -> Vec<CapabilityVersionBinding> {
        vec![CapabilityVersionBinding {
            package: self.id().into(),
            version: self.pack.manifest.version.to_string(),
            digest: self.pack.digest.clone(),
        }]
    }
    fn protect_task(&self, run: RunId) -> futures::future::BoxFuture<'_, Result<(), AgentError>> {
        let manager = self.manager.clone();
        let pin = DurablePackPin {
            id: format!(
                "{}.{run}.{}",
                crate::manager::hash(self.namespace.as_bytes()),
                self.id()
            ),
            scope: self.namespace.clone(),
            versions: vec![PackVersionRequirement {
                package: self.id().into(),
                version: self.pack.manifest.version.clone(),
                digest: self.pack.digest.clone(),
            }],
        };
        Box::pin(async move {
            tokio::task::spawn_blocking(move || manager.pin(pin))
                .await
                .map_err(|e| AgentError::Run(e.to_string()))?
                .map_err(|e| AgentError::Run(e.to_string()))
        })
    }
    fn release_task(&self, run: RunId) -> futures::future::BoxFuture<'_, Result<(), AgentError>> {
        let manager = self.manager.clone();
        let id = format!(
            "{}.{run}.{}",
            crate::manager::hash(self.namespace.as_bytes()),
            self.id()
        );
        Box::pin(async move {
            tokio::task::spawn_blocking(move || manager.unpin(&id))
                .await
                .map_err(|e| AgentError::Run(e.to_string()))?
                .map_err(|e| AgentError::Run(e.to_string()))
        })
    }
    fn plugins(&self) -> Vec<Arc<dyn Plugin>> {
        let runtime_id = format!("{}.runtime", self.id());
        let shared = Arc::new(SelectedRuntime {
            selection: self.clone(),
            instance: OnceLock::new(),
        });
        let mut plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(RuntimePlugin {
            id: runtime_id.clone(),
            runtime: shared.clone(),
        })];
        for plugin in &self.pack.manifest.plugins {
            if !self.selected_plugins.contains(plugin) {
                continue;
            }
            plugins.push(Arc::new(DeclaredPlugin {
                id: plugin.clone(),
                runtime_id: runtime_id.clone(),
            }));
        }
        for declaration in &self.pack.manifest.mcp {
            if !self.selected_mcp.contains(&declaration.id) {
                continue;
            }
            plugins.push(Arc::new(ManagedMcpPlugin {
                id: declaration.id.clone(),
                runtime_id: runtime_id.clone(),
                runtime: shared.clone(),
                declaration: declaration.clone(),
            }));
        }
        for declaration in &self.pack.manifest.implementations {
            if self.selected.contains(&declaration.id) {
                plugins.push(Arc::new(RemoteMemberPlugin {
                    declaration: declaration.clone(),
                    runtime_id: runtime_id.clone(),
                    runtime: shared.clone(),
                }));
            }
        }
        plugins
    }
}
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct CapabilityContributionSelection {
    pub implementations: Option<Vec<String>>,
    pub plugins: Option<Vec<String>>,
    pub pages: Option<Vec<String>>,
    pub mcp: Option<Vec<String>>,
    pub skills: Option<Vec<String>>,
}
struct PausedInstances(Vec<Arc<BackendInstance>>);
impl Drop for PausedInstances {
    fn drop(&mut self) {
        for instance in &self.0 {
            instance.resume_admission();
        }
    }
}
struct SelectedRuntime {
    selection: ManagedCapabilityPack,
    instance: OnceLock<Arc<BackendInstance>>,
}
impl SelectedRuntime {
    fn instance(&self) -> Result<Arc<BackendInstance>, PluginError> {
        self.instance
            .get()
            .cloned()
            .ok_or_else(|| PluginError::message("package runtime has not installed"))
    }
}
struct RuntimePlugin {
    id: String,
    runtime: Arc<SelectedRuntime>,
}
#[async_trait]
impl Plugin for RuntimePlugin {
    fn id(&self) -> &str {
        &self.id
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let selection = &self.runtime.selection;
        let instance = BackendInstance::new_selected(
            selection.manager.clone(),
            selection.pack.clone(),
            &selection.scope,
            &selection.configuration_version,
            selection.configuration.clone(),
            selection.selected.clone(),
        )
        .map_err(|e| PluginError::message(e.to_string()))?;
        instance
            .select_plugins(selection.selected_plugins.clone())
            .map_err(|e| PluginError::message(e.to_string()))?;
        instance
            .select_pages(selection.selected_pages.clone())
            .map_err(|e| PluginError::message(e.to_string()))?;
        *instance
            .data_scope
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = selection.namespace.clone();
        self.runtime
            .instance
            .set(instance.clone())
            .map_err(|_| PluginError::message("runtime reused across builds"))?;
        let fallback = instance.clone();
        context.on_cleanup(move || {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = fallback.shutdown().await;
                });
            }
        });
        context.contribute(CapabilityInstanceService(vec![instance.clone()]))?;
        context.on_shutdown(
            move || async move { instance.shutdown().await.map_err(|e| e.to_string()) },
        );
        Ok(())
    }
    async fn ready(&self, _: &mut PluginReadyContext<'_>) -> Result<(), PluginError> {
        let selection = &self.runtime.selection;
        if selection.pack.manifest.backend.is_some()
            && (!selection.selected.is_empty()
                || !selection.selected_plugins.is_empty()
                || !selection.selected_pages.is_empty())
        {
            self.runtime
                .instance()?
                .peer()
                .await
                .map_err(|e| PluginError::message(e.to_string()))?;
        }
        Ok(())
    }
}
struct DeclaredPlugin {
    id: String,
    runtime_id: String,
}
#[async_trait]
impl Plugin for DeclaredPlugin {
    fn id(&self) -> &str {
        &self.id
    }
    fn dependencies(&self) -> Vec<String> {
        vec![self.runtime_id.clone()]
    }
    async fn install(&self, _: &mut PluginContext<'_>) -> Result<(), PluginError> {
        Ok(())
    }
}
struct RemoteMemberPlugin {
    declaration: ExtensionImplementation,
    runtime_id: String,
    runtime: Arc<SelectedRuntime>,
}
/// Cross-process custom contracts are explicit stable IDs; no Rust Any crosses the wire.
pub struct RemoteExtensionService(pub RemoteImplementation);
#[async_trait]
impl Plugin for RemoteMemberPlugin {
    fn id(&self) -> &str {
        &self.declaration.id
    }
    fn dependencies(&self) -> Vec<String> {
        let mut dependencies = self.declaration.dependencies.clone();
        dependencies.push(self.runtime_id.clone());
        dependencies
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let implementation = Arc::new(RemoteImplementation {
            instance: self.runtime.instance()?,
            declaration: self.declaration.clone(),
        });
        context.contribute(RemoteExtensionService((*implementation).clone()))?;
        macro_rules! unique {
            ($service:ident,$contract:ident) => {{
                let binding: Arc<dyn $contract> = implementation.clone();
                context.provide($service(binding.clone()))?;
                context.contribute_extension(binding)
            }};
        }
        macro_rules! multiple {
            ($service:ident,$contract:ident) => {{
                let binding: Arc<dyn $contract> = implementation.clone();
                context.contribute($service(binding.clone()))?;
                context.contribute_extension(binding)
            }};
        }
        macro_rules! model {
            ($service:ident,$contract:ident) => {{
                let model: ModelRef =
                    serde_json::from_value(self.declaration.metadata["model"].clone())
                        .map_err(|e| PluginError::message(e.to_string()))?;
                let binding: Arc<dyn $contract> = implementation.clone();
                let mut registration = $service::new(model, binding.clone());
                if let Some(protocol) = self.declaration.metadata["protocol"].as_str() {
                    registration = registration.protocol(protocol);
                }
                if let Some(capabilities) = self.declaration.metadata.get("capabilities") {
                    registration = registration.capabilities(
                        serde_json::from_value(capabilities.clone())
                            .map_err(|e| PluginError::message(e.to_string()))?,
                    );
                }
                context.contribute(registration)?;
                context.contribute_extension(binding)
            }};
        }
        match self.declaration.extension.as_str() {
            "framework.agent-loop" => unique!(LoopService, AgentLoop),
            "framework.workflow-node" => multiple!(WorkflowNodeService, WorkflowNode),
            "framework.chat-model" => {
                let connection = self.declaration.metadata["connection"]
                    .as_str()
                    .ok_or_else(|| {
                        PluginError::message("chat implementation needs connection metadata")
                    })?;
                let mut models: Vec<ModelDescriptor> =
                    serde_json::from_value(self.declaration.metadata["models"].clone())
                        .map_err(|e| PluginError::message(e.to_string()))?;
                if let Some(discovery_id) = self.declaration.metadata["discovery"].as_str() {
                    let declaration = self
                        .runtime
                        .selection
                        .pack
                        .manifest
                        .implementations
                        .iter()
                        .find(|i| {
                            i.id == discovery_id
                                && i.extension == "framework.model-discovery"
                                && self.runtime.selection.selected.contains(&i.id)
                        })
                        .ok_or_else(|| {
                            PluginError::message("missing selected discovery implementation")
                        })?;
                    models = RemoteImplementation {
                        instance: implementation.instance.clone(),
                        declaration: declaration.clone(),
                    }
                    .discover()
                    .await
                    .map_err(|e| PluginError::message(e.to_string()))?;
                }
                let adapter: Arc<dyn ChatModel> = implementation.clone();
                let mut binding = ModelConnection::new(connection, adapter.clone(), models);
                if let Some(estimator_id) = self.declaration.metadata["token_estimator"].as_str() {
                    let declaration = self
                        .runtime
                        .selection
                        .pack
                        .manifest
                        .implementations
                        .iter()
                        .find(|i| {
                            i.id == estimator_id
                                && i.extension == "framework.token-estimator"
                                && self.runtime.selection.selected.contains(&i.id)
                        })
                        .ok_or_else(|| {
                            PluginError::message("missing token estimator implementation")
                        })?;
                    binding = binding.with_token_estimator(Arc::new(RemoteImplementation {
                        instance: implementation.instance.clone(),
                        declaration: declaration.clone(),
                    }));
                }
                context.contribute(ModelConnectionService(binding))?;
                context.contribute_extension(adapter)
            }
            "framework.model-discovery" => {
                context.contribute_extension::<dyn ModelDiscovery>(implementation)
            }
            "framework.model-selection" => unique!(ModelStrategyService, ModelSelectionStrategy),
            "framework.token-estimator" => Ok(()),
            "framework.embedding" => model!(EmbeddingModelService, EmbeddingModel),
            "framework.rerank" => model!(RerankModelService, RerankModel),
            "framework.image" => model!(ImageGenerationModelService, ImageGenerationModel),
            "framework.video" => model!(VideoGenerationModelService, VideoGenerationModel),
            "framework.speech-recognition" => {
                model!(SpeechRecognitionModelService, SpeechRecognitionModel)
            }
            "framework.speech-synthesis" => {
                model!(SpeechSynthesisModelService, SpeechSynthesisModel)
            }
            "framework.realtime" => model!(RealtimeModelService, RealtimeModel),
            "framework.tool-provider" => multiple!(ToolProviderService, ToolProvider),
            "framework.tool-policy" => multiple!(ToolPolicyService, ToolPolicy),
            "framework.context-provider" => multiple!(ContextService, ContextProvider),
            "framework.context-contributor" => {
                multiple!(ContextContributorService, ContextContributor)
            }
            "framework.context-assembler" => unique!(ContextAssemblerService, ContextAssembler),
            "framework.context-budget" => unique!(ContextBudgetPolicyService, ContextBudgetPolicy),
            "framework.context-reducer" => {
                if self.declaration.metadata["global_history"] == false {
                    context.contribute_extension::<dyn ContextReducer>(implementation)
                } else {
                    unique!(ContextReducerService, ContextReducer)
                }
            }
            "framework.skills" => context.contribute_extension::<dyn SkillProvider>(implementation),
            "framework.long-term-memory" => unique!(LongTermMemoryService, LongTermMemory),
            "framework.conversation-store" => unique!(ConversationStoreService, ConversationStore),
            "framework.state-store" => unique!(StateStoreService, StateStore),
            "context.summary-store" => unique!(SummaryStoreService, SummaryStore),
            "context.artifact-store" => unique!(ArtifactStoreService, ArtifactStore),
            "framework.interaction-store" => unique!(InteractionStoreService, InteractionStore),
            "framework.run-middleware" => multiple!(RunMiddlewareService, RunMiddleware),
            "framework.model-middleware" => multiple!(ModelMiddlewareService, ModelMiddleware),
            "framework.tool-middleware" => multiple!(ToolMiddlewareService, ToolMiddleware),
            "framework.node-middleware" => {
                multiple!(WorkflowNodeMiddlewareService, WorkflowNodeMiddleware)
            }
            "framework.lifecycle-hook" => multiple!(RunLifecycleHookService, RunLifecycleHook),
            "framework.event-sink" => {
                let sink: Arc<dyn EventSink> =
                    RemoteObservationSink::new((*implementation).clone());
                context.contribute(EventService(sink.clone()))?;
                context.contribute_extension(sink)
            }
            "framework.run-output" => {
                let factory: Arc<dyn RunOutputFactory> =
                    Arc::new(RemoteOutputFactory((*implementation).clone()));
                context.contribute(RunOutputService(factory.clone()))?;
                context.contribute_extension(factory)?;
                let sink: Arc<dyn EventSink> =
                    Arc::new(RemoteOutputSink((*implementation).clone()));
                context.contribute(EventService(sink))
            }
            extension if !extension.starts_with("framework.") => Ok(()),
            extension => Err(PluginError::message(format!(
                "unsupported framework contract: {extension}"
            ))),
        }
    }
}
#[derive(Clone)]
pub struct RemoteExtensionCall {
    pub implementation: String,
    pub operation: String,
    pub params: Value,
}
#[async_trait]
impl AgentOperation for RemoteExtensionCall {
    type Service = RemoteExtensionService;
    type Output = Value;
    fn select(&self, services: Vec<Arc<Self::Service>>) -> Result<Arc<Self::Service>, AgentError> {
        services
            .into_iter()
            .find(|service| service.0.declaration.id == self.implementation)
            .ok_or_else(|| {
                AgentError::InvalidConfiguration("remote implementation not selected".into())
            })
    }
    async fn execute(
        self,
        service: Arc<Self::Service>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<Value, AgentError> {
        tokio::select! {_ = cancellation.cancelled()=>Err(AgentError::Cancelled), result=service.0.invoke(&self.operation,self.params)=>result.map_err(|e|match e { RuntimeError::Indeterminate(message)=>AgentError::Indeterminate(message),error=>AgentError::Run(error.to_string()) })}
    }
}

pub struct CapabilityInstanceService(pub Vec<Arc<BackendInstance>>);
pub struct ListCapabilityInstances;
#[async_trait]
impl AgentOperation for ListCapabilityInstances {
    type Service = CapabilityInstanceService;
    type Output = Vec<InstanceIdentity>;
    fn select(&self, services: Vec<Arc<Self::Service>>) -> Result<Arc<Self::Service>, AgentError> {
        if services.is_empty() {
            return Err(AgentError::MissingService("CapabilityInstanceService"));
        }
        Ok(Arc::new(CapabilityInstanceService(
            services.into_iter().flat_map(|s| s.0.clone()).collect(),
        )))
    }
    async fn execute(
        self,
        service: Arc<Self::Service>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<Self::Output, AgentError> {
        Ok(service
            .0
            .iter()
            .filter(|instance| !instance.is_retired())
            .map(|instance| instance.identity.clone())
            .collect())
    }
}
pub struct CapabilityInstanceDirectory(pub Vec<Arc<BackendInstance>>);

struct ManagedMcpPlugin {
    id: String,
    runtime_id: String,
    runtime: Arc<SelectedRuntime>,
    declaration: McpDeclaration,
}
#[async_trait]
impl Plugin for ManagedMcpPlugin {
    fn id(&self) -> &str {
        &self.id
    }
    fn dependencies(&self) -> Vec<String> {
        vec![self.runtime_id.clone()]
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        use eliteforge_agent_official_plugin_tools_mcp::{McpConfig, McpToolsPlugin, McpTransport};
        let instance = self.runtime.instance()?;
        let transport = match &self.declaration.transport {
            McpDeclarationTransport::Stdio { program, args } => {
                let program =
                    self.runtime.selection.pack.path.join(
                        safe_relative(program).map_err(|e| PluginError::message(e.to_string()))?,
                    );
                McpTransport::Stdio {
                    program,
                    args: args.clone(),
                    env: std::collections::HashMap::new(),
                    current_dir: Some(
                        instance
                            .data_directory()
                            .map_err(|e| PluginError::message(e.to_string()))?,
                    ),
                    inherit_env: true,
                }
            }
            McpDeclarationTransport::Http { url } => McpTransport::StreamableHttp {
                url: url.clone(),
                headers: Default::default(),
            },
        };
        let namespace = format!("p_{}", &crate::manager::hash(self.id.as_bytes())[..16]);
        McpToolsPlugin::new(&self.id, McpConfig::new(namespace, transport))
            .install(context)
            .await
    }
}
