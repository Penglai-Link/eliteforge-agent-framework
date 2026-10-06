//! Standard Agent Skills integration.
//!
//! The default provider discovers system-level and local `SKILL.md` directories.
//! It contributes a metadata-only context message and normal model tools that
//! load instructions and resources only when they are selected.

mod filesystem;
mod tools;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::PathBuf,
    path::{Component, Path},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

use async_trait::async_trait;
use eliteforge_agent_core::{
    AgentError, CapabilitySkillSource, ContextBlock, ContextContributor, ContextContributorService,
    ContextInput, ContextSourceKind, Message, Plugin, PluginContext, PluginError,
    PluginReadyContext, Role, RunContext, RunMiddleware, RunNext, RunResult, SkillCatalog,
    SkillDetail, SkillProvider, SkillResource, SkillSummary, StateStore, ToolLease, ToolProvider,
    ToolProviderService,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::OnceCell;

pub use filesystem::{FileSystemSkillProvider, FileSystemSkillsConfig};

static EMBEDDED_SKILLS_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Restricts the catalog made available to one Agent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SkillSelection {
    /// Every valid discovered skill is available.
    #[default]
    All,
    /// Only these skill names are available. An empty set intentionally exposes
    /// no skills; unknown names fail before model dispatch for that execution.
    Only(BTreeSet<String>),
}

/// Official plugin that presents one [`SkillProvider`] through context and
/// tools. A custom provider can replace the file-system implementation without
/// changing the Agent loop or tool contracts.
pub struct SkillsPlugin {
    provider: Option<Arc<dyn SkillProvider>>,
    selection: SkillSelection,
}

impl Clone for SkillsPlugin {
    fn clone(&self) -> Self {
        Self {
            provider: self.provider.clone(),
            selection: self.selection.clone(),
        }
    }
}

impl Default for SkillsPlugin {
    fn default() -> Self {
        Self::filesystem(FileSystemSkillsConfig::default())
    }
}

impl SkillsPlugin {
    /// Use a custom provider, such as one backed by an application RAG index.
    pub fn new(provider: impl SkillProvider + 'static) -> Self {
        Self::from_arc(Arc::new(provider))
    }

    /// Use an already shared custom provider.
    pub fn from_arc(provider: Arc<dyn SkillProvider>) -> Self {
        Self {
            provider: Some(provider),
            selection: SkillSelection::All,
        }
    }

    /// Expose skills contributed by selected capability packages without adding
    /// a filesystem source of its own.
    pub fn sources_only() -> Self {
        Self {
            provider: None,
            selection: SkillSelection::All,
        }
    }

    /// Use the standard filesystem provider with explicit directory settings.
    pub fn filesystem(config: FileSystemSkillsConfig) -> Self {
        Self::new(FileSystemSkillProvider::new(config))
    }

    /// Restrict this Agent to a subset of discovered skills.
    pub fn only_skills<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.selection = SkillSelection::Only(names.into_iter().map(Into::into).collect());
        self
    }

    /// Re-enable every discovered skill after a previous [`Self::only_skills`].
    pub fn all_skills(mut self) -> Self {
        self.selection = SkillSelection::All;
        self
    }

    /// Access the configured provider for application diagnostics or reuse.
    pub fn provider(&self) -> Arc<dyn SkillProvider> {
        self.provider
            .clone()
            .unwrap_or_else(|| Arc::new(EmptySkillProvider))
    }
}

#[async_trait]
impl Plugin for SkillsPlugin {
    fn id(&self) -> &str {
        "skills.default"
    }

    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let runtime = Arc::new(SkillsRuntime::new(self.selection.clone()));
        context.provide(SkillsRuntimeService(runtime.clone()))?;
        if let Some(provider) = &self.provider {
            context.contribute_extension(provider.clone())?;
        }
        let endpoint = Arc::new(SkillsEndpoint { runtime });
        let context_provider: Arc<dyn ContextContributor> = endpoint.clone();
        let tool_provider: Arc<dyn ToolProvider> = endpoint.clone();
        let run_middleware: Arc<dyn RunMiddleware> = endpoint;
        context.contribute(ContextContributorService(context_provider.clone()))?;
        context.contribute_extension(context_provider)?;
        context.contribute(ToolProviderService(tool_provider.clone()))?;
        context.contribute_extension(tool_provider)?;
        context.contribute(eliteforge_agent_core::RunMiddlewareService(
            run_middleware.clone(),
        ))?;
        context.contribute_extension(run_middleware)
    }

    async fn ready(&self, context: &mut PluginReadyContext<'_>) -> Result<(), PluginError> {
        let providers = context
            .services()
            .extension_providers::<dyn SkillProvider>()
            .into_iter()
            .map(str::to_owned)
            .zip(context.extensions::<dyn SkillProvider>())
            .collect();
        context
            .services()
            .get::<SkillsRuntimeService>()
            .ok_or_else(|| PluginError::message("skills runtime service is missing"))?
            .0
            .set_provider(Arc::new(AggregatedSkillProvider::new(providers)))
    }
}

struct SkillsRuntimeService(Arc<SkillsRuntime>);

struct SkillsRuntime {
    provider: RwLock<Option<Arc<dyn SkillProvider>>>,
    selection: SkillSelection,
    snapshots: Mutex<SelectedSkillsSnapshots>,
    recovering: Mutex<HashMap<eliteforge_agent_core::RunId, RecoveryAttempt>>,
}

struct SelectedSkills {
    access: Option<BTreeSet<String>>,
    catalog: SkillCatalog,
    skills: BTreeMap<String, SkillSummary>,
    loaded: Mutex<BTreeMap<String, BTreeSet<String>>>,
}

struct RecoveryAttempt {
    retained: bool,
    loaded_skills: BTreeSet<String>,
}

type SelectedSkillsSlot = Arc<OnceCell<Arc<SelectedSkills>>>;
type SelectedSkillsSnapshots = HashMap<eliteforge_agent_core::RunId, SelectedSkillsSlot>;

impl SkillsRuntime {
    fn new(selection: SkillSelection) -> Self {
        Self {
            provider: RwLock::new(None),
            selection,
            snapshots: Mutex::new(HashMap::new()),
            recovering: Mutex::new(HashMap::new()),
        }
    }

    fn set_provider(&self, provider: Arc<dyn SkillProvider>) -> Result<(), PluginError> {
        let mut slot = self
            .provider
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.is_some() {
            return Err(PluginError::message(
                "skills runtime is already initialized",
            ));
        }
        *slot = Some(provider);
        Ok(())
    }

    fn provider(&self) -> Result<Arc<dyn SkillProvider>, AgentError> {
        self.provider
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .ok_or_else(|| AgentError::InvalidConfiguration("skills runtime is not ready".into()))
    }

    async fn snapshot(&self, context: &RunContext) -> Result<Arc<SelectedSkills>, AgentError> {
        let run_id = context.run_id;
        let slot = self
            .snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(run_id)
            .or_insert_with(|| Arc::new(OnceCell::new()))
            .clone();
        let snapshot = slot
            .get_or_try_init(|| async {
                let catalog = self.provider()?.catalog(context).await?;
                Ok::<_, AgentError>(Arc::new(self.select(catalog, context)?))
            })
            .await?;
        let access = context
            .extensions
            .get::<eliteforge_agent_core::SkillAccess>()
            .map(|access| access.names.clone());
        if snapshot.access != access {
            return Err(AgentError::InvalidConfiguration(
                "skill permissions changed for an existing logical run".into(),
            ));
        }
        Ok(snapshot.clone())
    }

    async fn begin_recovery(
        &self,
        context: &RunContext,
        state: Option<&dyn StateStore>,
    ) -> Result<(), AgentError> {
        let retained = self
            .snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(&context.run_id);
        let loaded_skills = if retained {
            BTreeSet::new()
        } else if let Some(state) = state {
            successful_loaded_skill_names(&state.load(context.run_id).await?.transcript)
        } else {
            BTreeSet::new()
        };
        self.recovering
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                context.execution_id(),
                RecoveryAttempt {
                    retained,
                    loaded_skills,
                },
            );
        Ok(())
    }

    fn finish_recovery(&self, context: &RunContext) {
        self.recovering
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&context.execution_id());
    }

    fn needs_recovery_index(&self, context: &RunContext, name: &str) -> bool {
        self.recovering
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&context.execution_id())
            .is_some_and(|recovery| !recovery.retained && recovery.loaded_skills.contains(name))
    }

    fn select(
        &self,
        mut catalog: SkillCatalog,
        context: &RunContext,
    ) -> Result<SelectedSkills, AgentError> {
        let mut discovered = BTreeMap::new();
        for skill in &catalog.skills {
            if skill.name.trim().is_empty() {
                return Err(AgentError::InvalidConfiguration(
                    "SkillProvider returned a skill with an empty name".into(),
                ));
            }
            if discovered
                .insert(skill.name.clone(), skill.clone())
                .is_some()
            {
                return Err(AgentError::InvalidConfiguration(format!(
                    "SkillProvider returned duplicate skill `{}`",
                    skill.name
                )));
            }
        }

        let mut skills: BTreeMap<String, SkillSummary> = match &self.selection {
            SkillSelection::All => discovered,
            SkillSelection::Only(names) => {
                let missing = names
                    .iter()
                    .filter(|name| !discovered.contains_key(*name))
                    .cloned()
                    .collect::<Vec<_>>();
                if !missing.is_empty() {
                    return Err(AgentError::InvalidConfiguration(format!(
                        "configured skills were not discovered: {}",
                        missing.join(", ")
                    )));
                }
                names
                    .iter()
                    .filter_map(|name| {
                        discovered
                            .get(name)
                            .cloned()
                            .map(|skill| (name.clone(), skill))
                    })
                    .collect()
            }
        };
        if let Some(access) = context
            .extensions
            .get::<eliteforge_agent_core::SkillAccess>()
        {
            skills.retain(|name, _| access.allows(name));
            catalog.skills.retain(|skill| access.allows(&skill.name));
        }
        Ok(SelectedSkills {
            access: context
                .extensions
                .get::<eliteforge_agent_core::SkillAccess>()
                .map(|access| access.names.clone()),
            catalog,
            skills,
            loaded: Mutex::new(BTreeMap::new()),
        })
    }

    async fn load(&self, context: &RunContext, name: &str) -> Result<SkillDetail, AgentError> {
        let snapshot = self.snapshot(context).await?;
        let expected = snapshot.skills.get(name).cloned().ok_or_else(|| {
            AgentError::Run(format!("skill `{name}` is not enabled for this execution"))
        })?;
        let mut detail = self.provider()?.load(context, name).await?;
        if detail.skill.name != name {
            return Err(AgentError::Run(format!(
                "SkillProvider returned details for `{}` while `{name}` was requested",
                detail.skill.name
            )));
        }
        let mut files = BTreeSet::new();
        for file in &detail.files {
            if validate_resource_path(&file.path).is_err() || !files.insert(file.path.clone()) {
                return Err(AgentError::Run(format!(
                    "SkillProvider returned an invalid resource list for `{name}`"
                )));
            }
        }
        // The metadata shown to the model remains the immutable execution
        // snapshot even when a custom provider decorates its loaded document.
        detail.skill = expected;
        snapshot
            .loaded
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(name.to_owned(), files);
        Ok(detail)
    }

    async fn read_resource(
        &self,
        context: &RunContext,
        name: &str,
        path: &str,
    ) -> Result<SkillResource, AgentError> {
        let snapshot = self.snapshot(context).await?;
        if !snapshot.skills.contains_key(name) {
            return Err(AgentError::Run(format!(
                "skill `{name}` is not enabled for this execution"
            )));
        }
        validate_resource_path(path)?;
        let was_loaded = snapshot
            .loaded
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(name);
        if !was_loaded && self.needs_recovery_index(context, name) {
            // A persisted tool batch can resume after this plugin has been
            // reconstructed. Only a persisted successful skill_load can restore
            // the resource index; normal calls and in-process missing loads fail.
            self.load(context, name).await?;
        }
        let listed = {
            let loaded = snapshot
                .loaded
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(files) = loaded.get(name) else {
                return Err(AgentError::Run(format!(
                    "load skill `{name}` with skill_load before reading its resources"
                )));
            };
            files.contains(path)
        };
        if !listed {
            return Err(AgentError::Run(format!(
                "resource `{path}` is not listed by loaded skill `{name}`"
            )));
        }
        let resource = self.provider()?.read_resource(context, name, path).await?;
        if resource.skill_name != name || resource.path != path {
            return Err(AgentError::Run(format!(
                "SkillProvider returned a different resource while `{name}/{path}` was requested"
            )));
        }
        Ok(resource)
    }

    async fn diagnostics(
        &self,
        context: &RunContext,
    ) -> Result<Vec<eliteforge_agent_core::SkillDiagnostic>, AgentError> {
        Ok(self.snapshot(context).await?.catalog.diagnostics.clone())
    }

    fn release(&self, context: &RunContext) {
        self.snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&context.run_id);
        self.finish_recovery(context);
        if let Ok(provider) = self.provider() {
            provider.release(context);
        }
    }
}

/// A `SkillProvider` that combines selected package sources into one stable
/// per-run catalog. Name collisions are rejected before a model can load an
/// ambiguous skill.
#[derive(Clone)]
pub struct AggregatedSkillProvider {
    providers: Vec<(String, Arc<dyn SkillProvider>)>,
    snapshots: Arc<Mutex<AggregateSnapshots>>,
}

type AggregateSnapshotSlot = Arc<OnceCell<Arc<AggregateSnapshot>>>;
type AggregateSnapshots = HashMap<eliteforge_agent_core::RunId, AggregateSnapshotSlot>;

struct AggregateSnapshot {
    catalog: SkillCatalog,
    routes: BTreeMap<String, Arc<dyn SkillProvider>>,
}

impl AggregatedSkillProvider {
    pub fn new(providers: Vec<(String, Arc<dyn SkillProvider>)>) -> Self {
        Self {
            providers,
            snapshots: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn snapshot(&self, context: &RunContext) -> Result<Arc<AggregateSnapshot>, AgentError> {
        let slot = self
            .snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(context.run_id)
            .or_insert_with(|| Arc::new(OnceCell::new()))
            .clone();
        let providers = self.providers.clone();
        let snapshot = slot
            .get_or_try_init(|| async move {
                let mut catalog = SkillCatalog::default();
                let mut routes = BTreeMap::new();
                let mut owners = BTreeMap::new();
                for (provider_id, provider) in providers {
                    let source_catalog = provider.catalog(context).await?;
                    catalog.diagnostics.extend(source_catalog.diagnostics);
                    for skill in source_catalog.skills {
                        let name = skill.name.clone();
                        if let Some(existing) = owners.insert(name.clone(), provider_id.clone()) {
                            return Err(AgentError::InvalidConfiguration(format!(
                                "skill `{name}` is provided by both `{existing}` and `{provider_id}`"
                            )));
                        }
                        routes.insert(name, provider.clone());
                        catalog.skills.push(skill);
                    }
                }
                catalog.skills.sort_by(|left, right| left.name.cmp(&right.name));
                Ok::<_, AgentError>(Arc::new(AggregateSnapshot { catalog, routes }))
            })
            .await?;
        Ok(snapshot.clone())
    }
}

#[async_trait]
impl SkillProvider for AggregatedSkillProvider {
    async fn catalog(&self, context: &RunContext) -> Result<SkillCatalog, AgentError> {
        Ok(self.snapshot(context).await?.catalog.clone())
    }

    async fn load(&self, context: &RunContext, name: &str) -> Result<SkillDetail, AgentError> {
        let snapshot = self.snapshot(context).await?;
        let provider = snapshot.routes.get(name).ok_or_else(|| {
            AgentError::Run(format!(
                "skill `{name}` is not available from the selected providers"
            ))
        })?;
        provider.load(context, name).await
    }

    async fn read_resource(
        &self,
        context: &RunContext,
        name: &str,
        path: &str,
    ) -> Result<SkillResource, AgentError> {
        let snapshot = self.snapshot(context).await?;
        let provider = snapshot.routes.get(name).ok_or_else(|| {
            AgentError::Run(format!(
                "skill `{name}` is not available from the selected providers"
            ))
        })?;
        provider.read_resource(context, name, path).await
    }

    fn release(&self, context: &RunContext) {
        self.snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&context.run_id);
        let mut released = Vec::<Arc<dyn SkillProvider>>::new();
        for (_, provider) in &self.providers {
            if released
                .iter()
                .any(|previous| Arc::ptr_eq(previous, provider))
            {
                continue;
            }
            provider.release(context);
            released.push(provider.clone());
        }
    }
}

struct EmptySkillProvider;

#[async_trait]
impl SkillProvider for EmptySkillProvider {
    async fn catalog(&self, _: &RunContext) -> Result<SkillCatalog, AgentError> {
        Ok(SkillCatalog::default())
    }

    async fn load(&self, _: &RunContext, name: &str) -> Result<SkillDetail, AgentError> {
        Err(AgentError::Run(format!("skill `{name}` is not available")))
    }

    async fn read_resource(
        &self,
        _: &RunContext,
        name: &str,
        _: &str,
    ) -> Result<SkillResource, AgentError> {
        Err(AgentError::Run(format!("skill `{name}` is not available")))
    }
}

#[derive(Clone)]
struct LabeledSkillProvider {
    label: String,
    inner: Arc<dyn SkillProvider>,
}

impl LabeledSkillProvider {
    fn new(label: impl Into<String>, inner: Arc<dyn SkillProvider>) -> Self {
        Self {
            label: label.into(),
            inner,
        }
    }
}

#[async_trait]
impl SkillProvider for LabeledSkillProvider {
    async fn catalog(&self, context: &RunContext) -> Result<SkillCatalog, AgentError> {
        let mut catalog = self.inner.catalog(context).await?;
        for skill in &mut catalog.skills {
            skill.source = self.label.clone();
        }
        Ok(catalog)
    }

    async fn load(&self, context: &RunContext, name: &str) -> Result<SkillDetail, AgentError> {
        let mut detail = self.inner.load(context, name).await?;
        detail.skill.source = self.label.clone();
        Ok(detail)
    }

    async fn read_resource(
        &self,
        context: &RunContext,
        name: &str,
        path: &str,
    ) -> Result<SkillResource, AgentError> {
        self.inner.read_resource(context, name, path).await
    }

    fn release(&self, context: &RunContext) {
        self.inner.release(context);
    }
}

/// Registers one source declared by a selected capability package.
pub struct CapabilitySkillsSourcePlugin {
    id: String,
    pack_id: String,
    source: CapabilitySkillSource,
    resource_root: Option<PathBuf>,
}

impl CapabilitySkillsSourcePlugin {
    pub fn new(
        id: impl Into<String>,
        pack_id: impl Into<String>,
        source: CapabilitySkillSource,
        resource_root: Option<PathBuf>,
    ) -> Self {
        Self {
            id: id.into(),
            pack_id: pack_id.into(),
            source,
            resource_root,
        }
    }

    fn directory(&self) -> Result<PathBuf, PluginError> {
        match &self.source {
            CapabilitySkillSource::Directory(directory) => Ok(directory.clone()),
            CapabilitySkillSource::Embedded(bundle) => self
                .resource_root
                .as_deref()
                .ok_or_else(|| {
                    PluginError::message(format!(
                        "capability package `{}` embeds skills; configure FrameworkBuilder::capability_skills_root(...) to prepare them",
                        self.pack_id
                    ))
                })
                .and_then(|root| prepare_embedded_skills(root, &self.pack_id, bundle)),
        }
    }
}

#[async_trait]
impl Plugin for CapabilitySkillsSourcePlugin {
    fn id(&self) -> &str {
        &self.id
    }

    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let directory = self.directory()?;
        let provider: Arc<dyn SkillProvider> = Arc::new(LabeledSkillProvider::new(
            format!("capability-pack:{}", self.pack_id),
            Arc::new(FileSystemSkillProvider::new(
                FileSystemSkillsConfig::new()
                    .without_system_directory()
                    .with_local_directory(directory),
            )),
        ));
        context.contribute_extension(provider)
    }
}

fn prepare_embedded_skills(
    resource_root: &Path,
    pack_id: &str,
    bundle: &eliteforge_agent_core::EmbeddedCapabilitySkills,
) -> Result<PathBuf, PluginError> {
    if bundle.files.is_empty() || bundle.content_hash.trim().is_empty() {
        return Err(PluginError::message(format!(
            "capability package `{pack_id}` has an empty embedded skills bundle"
        )));
    }
    let content_digest = embedded_content_digest(bundle)?;
    if is_sha256_digest(bundle.content_hash)
        && !bundle.content_hash.eq_ignore_ascii_case(&content_digest)
    {
        return Err(PluginError::message(format!(
            "embedded skills content digest for capability package `{pack_id}` does not match its files"
        )));
    }
    let destination = resource_root
        .join("capability-packs")
        .join(safe_cache_component(pack_id))
        .join(&content_digest)
        .join("skills");
    match fs::metadata(&destination) {
        Ok(metadata) if metadata.is_dir() => return Ok(destination),
        Ok(_) => {
            return Err(PluginError::message(format!(
                "embedded skills destination `{}` is not a directory",
                destination.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(PluginError::message(format!(
                "inspect embedded skills destination `{}` failed: {error}",
                destination.display()
            )));
        }
    }

    let parent = destination.parent().ok_or_else(|| {
        PluginError::message("embedded skills destination has no parent directory")
    })?;
    fs::create_dir_all(parent).map_err(|error| {
        PluginError::message(format!(
            "create embedded skills cache directory `{}` failed: {error}",
            parent.display()
        ))
    })?;
    let temporary = create_embedded_skills_temporary(parent, &content_digest)?;

    let write_result = (|| -> Result<(), PluginError> {
        for file in bundle.files {
            let relative = safe_skill_path(file.path)?;
            let output = temporary.join(relative);
            let output_parent = output.parent().ok_or_else(|| {
                PluginError::message(format!("embedded skill path `{}` has no parent", file.path))
            })?;
            fs::create_dir_all(output_parent).map_err(|error| {
                PluginError::message(format!(
                    "create embedded skill resource directory `{}` failed: {error}",
                    output_parent.display()
                ))
            })?;
            fs::write(&output, file.contents).map_err(|error| {
                PluginError::message(format!(
                    "write embedded skill resource `{}` failed: {error}",
                    output.display()
                ))
            })?;
        }
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_dir_all(&temporary);
        return Err(error);
    }

    match fs::rename(&temporary, &destination) {
        Ok(()) => Ok(destination),
        Err(_) if destination.is_dir() => {
            let _ = fs::remove_dir_all(&temporary);
            Ok(destination)
        }
        Err(error) => {
            let _ = fs::remove_dir_all(&temporary);
            Err(PluginError::message(format!(
                "publish embedded skills directory `{}` failed: {error}",
                destination.display()
            )))
        }
    }
}

fn create_embedded_skills_temporary(
    parent: &Path,
    content_digest: &str,
) -> Result<PathBuf, PluginError> {
    for _ in 0..128 {
        let sequence = EMBEDDED_SKILLS_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".skills-{}-{}-{sequence}",
            std::process::id(),
            content_digest
        ));
        match fs::create_dir(&temporary) {
            Ok(()) => return Ok(temporary),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(PluginError::message(format!(
                    "create temporary embedded skills directory `{}` failed: {error}",
                    temporary.display()
                )));
            }
        }
    }
    Err(PluginError::message(format!(
        "create a unique temporary embedded skills directory below `{}` failed after 128 attempts",
        parent.display()
    )))
}

fn safe_skill_path(path: &str) -> Result<&Path, PluginError> {
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(PluginError::message(format!(
            "embedded skill resource path `{}` is not a normal relative path",
            path.display()
        )));
    }
    Ok(path)
}

fn embedded_content_digest(
    bundle: &eliteforge_agent_core::EmbeddedCapabilitySkills,
) -> Result<String, PluginError> {
    let mut files = bundle.files.iter().collect::<Vec<_>>();
    files.sort_by(|left, right| left.path.cmp(right.path));

    let mut paths = BTreeSet::new();
    let mut digest = Sha256::new();
    for file in files {
        safe_skill_path(file.path)?;
        if !paths.insert(file.path) {
            return Err(PluginError::message(format!(
                "embedded skill resource path `{}` is declared more than once",
                file.path
            )));
        }
        digest.update(file.path.as_bytes());
        digest.update([0_u8]);
        digest.update(file.contents);
        digest.update([0_u8]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn is_sha256_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f' | b'A'..=b'F'))
}

fn safe_cache_component(value: &str) -> String {
    value
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod embedded_resource_tests {
    use super::*;
    use eliteforge_agent_core::{CapabilitySkillFile, EmbeddedCapabilitySkills};
    use std::sync::{Arc, Barrier};

    const FIRST_FILES: &[CapabilitySkillFile] = &[
        CapabilitySkillFile {
            path: "review/SKILL.md",
            contents: b"---\nname: review\ndescription: Review\n---\nReview it.\n",
        },
        CapabilitySkillFile {
            path: "review/references/checklist.md",
            contents: b"- Check the source.\n",
        },
    ];
    const SECOND_FILES: &[CapabilitySkillFile] = &[CapabilitySkillFile {
        path: "review/SKILL.md",
        contents: b"---\nname: review\ndescription: Review v2\n---\nReview it again.\n",
    }];
    const INVALID_FILES: &[CapabilitySkillFile] = &[CapabilitySkillFile {
        path: "../outside.txt",
        contents: b"must not escape",
    }];
    const DUPLICATE_FILES: &[CapabilitySkillFile] = &[
        CapabilitySkillFile {
            path: "review/SKILL.md",
            contents: b"first",
        },
        CapabilitySkillFile {
            path: "review/SKILL.md",
            contents: b"second",
        },
    ];
    const FIRST: EmbeddedCapabilitySkills = EmbeddedCapabilitySkills::new(FIRST_FILES, "a1");
    const SECOND: EmbeddedCapabilitySkills = EmbeddedCapabilitySkills::new(SECOND_FILES, "b2");
    const INVALID: EmbeddedCapabilitySkills = EmbeddedCapabilitySkills::new(INVALID_FILES, "bad");
    const DUPLICATE: EmbeddedCapabilitySkills =
        EmbeddedCapabilitySkills::new(DUPLICATE_FILES, "duplicate");
    const WRONG_DIGEST: EmbeddedCapabilitySkills = EmbeddedCapabilitySkills::new(
        FIRST_FILES,
        "0000000000000000000000000000000000000000000000000000000000000000",
    );

    #[test]
    fn embedded_resource_preparation_is_atomic_reusable_and_versioned() {
        let root = tempfile::tempdir().unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let first_root = root.path().to_path_buf();
        let second_root = root.path().to_path_buf();
        let first_barrier = barrier.clone();
        let first = std::thread::spawn(move || {
            first_barrier.wait();
            prepare_embedded_skills(&first_root, "shop.pack", &FIRST)
        });
        let second = std::thread::spawn(move || {
            barrier.wait();
            prepare_embedded_skills(&second_root, "shop.pack", &FIRST)
        });
        let first = first.join().unwrap().unwrap();
        let second = second.join().unwrap().unwrap();
        assert_eq!(first, second);
        assert_eq!(
            fs::read_to_string(first.join("review").join("SKILL.md")).unwrap(),
            "---\nname: review\ndescription: Review\n---\nReview it.\n"
        );

        let second_version = prepare_embedded_skills(root.path(), "shop.pack", &SECOND).unwrap();
        assert_ne!(first, second_version);
        assert!(first.is_dir());
        assert!(second_version.is_dir());

        let error = prepare_embedded_skills(root.path(), "shop.pack", &INVALID)
            .expect_err("invalid embedded resource paths must not be published");
        assert!(error.to_string().contains("normal relative path"));
        let error = prepare_embedded_skills(root.path(), "shop.pack", &DUPLICATE)
            .expect_err("duplicate embedded resource paths must be rejected");
        assert!(error.to_string().contains("declared more than once"));
        let error = prepare_embedded_skills(root.path(), "shop.pack", &WRONG_DIGEST)
            .expect_err("a declared SHA-256 digest must match its embedded resources");
        assert!(error.to_string().contains("content digest"));
        let cache = root.path().join("capability-packs");
        let temporary_entries = fs::read_dir(cache)
            .unwrap()
            .flat_map(|entry| fs::read_dir(entry.unwrap().path()).unwrap())
            .flat_map(|entry| fs::read_dir(entry.unwrap().path()).unwrap())
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".skills-"))
            .count();
        assert_eq!(temporary_entries, 0);
    }
}

fn successful_loaded_skill_names(transcript: &[Message]) -> BTreeSet<String> {
    let mut calls = HashMap::new();
    let mut loaded = BTreeSet::new();
    for message in transcript {
        for call in &message.tool_calls {
            if call.call_name != "skill_load" {
                continue;
            }
            let Some(name) = call.arguments.get("name").and_then(|value| value.as_str()) else {
                continue;
            };
            if !name.is_empty() {
                calls.insert(call.id.clone(), name.to_owned());
            }
        }
        if message.role != Role::Tool
            || message
                .tool_result
                .as_ref()
                .is_none_or(|result| result.is_error)
        {
            continue;
        }
        let Some(call_id) = message.tool_call_id.as_deref() else {
            continue;
        };
        if let Some(name) = calls.remove(call_id) {
            loaded.insert(name);
        }
    }
    loaded
}

fn validate_resource_path(path: &str) -> Result<(), AgentError> {
    if path.trim() != path || path.is_empty() || Path::new(path).is_absolute() {
        return Err(AgentError::Run(
            "skill resource path must be a normal relative path".into(),
        ));
    }
    if Path::new(path)
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(AgentError::Run(
            "skill resource path must be a normal relative path".into(),
        ));
    }
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ContextSkill<'a> {
    name: &'a str,
    description: &'a str,
    source: &'a str,
    location: &'a str,
}

#[derive(Clone)]
struct SkillsEndpoint {
    runtime: Arc<SkillsRuntime>,
}

#[async_trait]
impl ContextContributor for SkillsEndpoint {
    fn kind(&self) -> ContextSourceKind {
        ContextSourceKind::Instruction
    }
    fn block_id(&self) -> &str {
        "skills.catalog"
    }
    async fn contribute(&self, input: &ContextInput<'_>) -> Result<ContextBlock, AgentError> {
        let context = input.run;
        // The initial tool batch of a resumed workflow runs before the next
        // model turn. Once a model turn begins, future resource reads must use
        // the normal skill_load path again.
        self.runtime.finish_recovery(context);
        let snapshot = self.runtime.snapshot(context).await?;
        if snapshot.skills.is_empty() {
            return Ok(ContextBlock::instructions(self.block_id(), Vec::new()));
        }
        let skills = snapshot
            .skills
            .values()
            .map(|skill| ContextSkill {
                name: &skill.name,
                description: &skill.description,
                source: &skill.source,
                location: &skill.location,
            })
            .collect::<Vec<_>>();
        let metadata = serialize_catalog_for_context(&skills)?;
        Ok(ContextBlock::instructions(
            self.block_id(),
            vec![Message::system(format!(
                "Available skills are metadata only. Load a skill with `skill_load` before following its instructions. After loading it, use `skill_read_resource` for a listed file when needed. Loading a skill never executes scripts.\n<available_skills>{metadata}</available_skills>"
            ))],
        ))
    }
}

fn serialize_catalog_for_context(skills: &[ContextSkill<'_>]) -> Result<String, AgentError> {
    serde_json::to_string(skills)
        .map(|catalog| {
            catalog
                .replace('&', "\\u0026")
                .replace('<', "\\u003c")
                .replace('>', "\\u003e")
        })
        .map_err(|_| AgentError::Run("serialize skill metadata for model context failed".into()))
}

#[async_trait]
impl ToolProvider for SkillsEndpoint {
    async fn lease(&self, context: &RunContext) -> Result<ToolLease, AgentError> {
        let snapshot = self.runtime.snapshot(context).await?;
        if snapshot.skills.is_empty() && snapshot.catalog.diagnostics.is_empty() {
            return Ok(ToolLease::new(Vec::new()));
        }
        Ok(ToolLease::new(tools::bindings(
            self.runtime.clone(),
            !snapshot.skills.is_empty(),
            !snapshot.catalog.diagnostics.is_empty(),
        )))
    }
}

#[async_trait]
impl RunMiddleware for SkillsEndpoint {
    async fn intercept(
        &self,
        request: &eliteforge_agent_core::RunRequest,
        context: &RunContext,
        next: RunNext<'_>,
    ) -> Result<RunResult, AgentError> {
        if request.resume.is_some() {
            self.runtime
                .begin_recovery(context, next.services.state.as_deref())
                .await?;
        }
        let result = next.run().await;
        self.runtime.finish_recovery(context);
        let retain_snapshot = result.as_ref().is_ok_and(|result| {
            matches!(
                &result.status,
                eliteforge_agent_core::RunStatus::AwaitingInput
            )
        });
        if !retain_snapshot {
            self.runtime.release(context);
        }
        result
    }
}
