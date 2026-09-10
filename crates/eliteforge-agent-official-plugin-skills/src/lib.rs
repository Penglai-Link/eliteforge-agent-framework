//! Standard Agent Skills integration.
//!
//! The default provider discovers system-level and local `SKILL.md` directories.
//! It contributes a metadata-only context message and normal model tools that
//! load instructions and resources only when they are selected.

mod filesystem;
mod tools;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Component, Path},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use eliteforge_agent_core::{
    AgentError, ContextProvider, ContextService, Message, Plugin, PluginContext, PluginError, Role,
    RunContext, RunMiddleware, RunNext, RunResult, SkillCatalog, SkillDetail, SkillProvider,
    SkillResource, SkillSummary, StateStore, ToolLease, ToolProvider, ToolProviderService,
};
use serde::Serialize;
use tokio::sync::OnceCell;

pub use filesystem::{FileSystemSkillProvider, FileSystemSkillsConfig};

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
#[derive(Clone)]
pub struct SkillsPlugin {
    provider: Arc<dyn SkillProvider>,
    selection: SkillSelection,
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
            provider,
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
        self.provider.clone()
    }
}

#[async_trait]
impl Plugin for SkillsPlugin {
    fn id(&self) -> &str {
        "skills.default"
    }

    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let runtime = Arc::new(SkillsRuntime::new(
            self.provider.clone(),
            self.selection.clone(),
        ));
        let endpoint = SkillsEndpoint { runtime };
        context.contribute(ContextService(Arc::new(endpoint.clone())))?;
        context.contribute(ToolProviderService(Arc::new(endpoint.clone())))?;
        context.contribute(eliteforge_agent_core::RunMiddlewareService(Arc::new(
            endpoint,
        )))
    }
}

struct SkillsRuntime {
    provider: Arc<dyn SkillProvider>,
    selection: SkillSelection,
    snapshots: Mutex<SelectedSkillsSnapshots>,
    recovering: Mutex<HashMap<eliteforge_agent_core::RunId, RecoveryAttempt>>,
}

struct SelectedSkills {
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
    fn new(provider: Arc<dyn SkillProvider>, selection: SkillSelection) -> Self {
        Self {
            provider,
            selection,
            snapshots: Mutex::new(HashMap::new()),
            recovering: Mutex::new(HashMap::new()),
        }
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
                let catalog = self.provider.catalog(context).await?;
                Ok::<_, AgentError>(Arc::new(self.select(catalog)?))
            })
            .await?;
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

    fn select(&self, catalog: SkillCatalog) -> Result<SelectedSkills, AgentError> {
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

        let skills = match &self.selection {
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
        Ok(SelectedSkills {
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
        let mut detail = self.provider.load(context, name).await?;
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
        let resource = self.provider.read_resource(context, name, path).await?;
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
        self.provider.release(context);
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
impl ContextProvider for SkillsEndpoint {
    async fn context(&self, context: &RunContext) -> Result<Vec<Message>, AgentError> {
        // The initial tool batch of a resumed workflow runs before the next
        // model turn. Once a model turn begins, future resource reads must use
        // the normal skill_load path again.
        self.runtime.finish_recovery(context);
        let snapshot = self.runtime.snapshot(context).await?;
        if snapshot.skills.is_empty() {
            return Ok(Vec::new());
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
        Ok(vec![Message::system(format!(
            "Available skills are metadata only. Load a skill with `skill_load` before following its instructions. After loading it, use `skill_read_resource` for a listed file when needed. Loading a skill never executes scripts.\n<available_skills>{metadata}</available_skills>"
        ))])
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
                    | eliteforge_agent_core::RunStatus::RecoveryRequired
            )
        });
        if !retain_snapshot {
            self.runtime.release(context);
        }
        result
    }
}
