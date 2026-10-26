//! Development fixtures only; not a storage option shipped to applications.
use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_storage_local::{SqliteConversationStore, SqliteStateStore};
use std::sync::Arc;
pub mod batch_outcomes;
#[allow(dead_code)]
mod conversation;
pub mod storage_contracts;
pub use conversation::*;

/// Fixture convenience: installs a Framework and explicitly selects all fixture
/// contributions. Applications configure their own AgentSelection instead.
#[derive(Clone)]
pub struct TestBuilder {
    framework: FrameworkBuilder,
    storage: bool,
    skills: Vec<String>,
}
impl Default for TestBuilder {
    fn default() -> Self {
        Self {
            framework: FrameworkBuilder::new(),
            storage: true,
            skills: Vec::new(),
        }
    }
}
impl TestBuilder {
    pub fn new() -> Self {
        Self::default().without_storage()
    }
    pub fn skills(mut self, names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.skills = names.into_iter().map(Into::into).collect();
        self
    }
    pub fn without_storage(mut self) -> Self {
        self.storage = false;
        self
    }
    pub fn default_models(mut self, enabled: bool) -> Self {
        self.framework = self.framework.default_models(enabled);
        self
    }
    pub fn plugin(mut self, plugin: impl Plugin + 'static) -> Self {
        self.framework = self.framework.plugin(plugin);
        self
    }
    pub fn plugin_arc(mut self, plugin: Arc<dyn Plugin>) -> Self {
        self.framework = self.framework.plugin_arc(plugin);
        self
    }
    pub fn plugin_factory(mut self, factory: ConfiguredPluginFactory) -> Self {
        self.framework = self.framework.plugin_factory(factory);
        self
    }
    pub fn capability_pack(mut self, pack: impl CapabilityPack + 'static) -> Self {
        self.framework = self.framework.capability_pack(pack);
        self
    }
    pub fn capability_pack_arc(mut self, pack: Arc<dyn CapabilityPack>) -> Self {
        self.framework = self.framework.capability_pack_arc(pack);
        self
    }
    pub fn capability_skills_root(mut self, root: impl Into<std::path::PathBuf>) -> Self {
        self.framework = self.framework.capability_skills_root(root);
        self
    }
    pub async fn build(self) -> Result<Agent, AgentError> {
        let builder = if self.storage {
            self.framework.plugin(TestStoragePlugin::default())
        } else {
            self.framework
        };
        let framework = builder.build().await?;
        let mut selection = match fixture_selection(&framework) {
            Ok(selection) => selection,
            Err(error) => {
                framework.close().await;
                return Err(error);
            }
        };
        selection.skills = Some(self.skills);
        match framework.agent(selection).build().await {
            Ok(agent) => Ok(agent),
            Err(error) => {
                framework.close().await;
                Err(error)
            }
        }
    }
}
/// Resolve fixture providers explicitly; ambiguity remains a configuration error.
pub fn fixture_selection(framework: &Framework) -> Result<AgentSelection, AgentError> {
    let registry = framework.services()?;
    fn one(ids: Vec<&str>) -> Result<Option<String>, AgentError> {
        match ids.len() {
            0 => Ok(None),
            1 => Ok(ids.into_iter().next().map(str::to_owned)),
            _ => Err(AgentError::InvalidConfiguration(format!(
                "fixture needs an explicit selection: {ids:?}"
            ))),
        }
    }
    Ok(AgentSelection {
        agent_loop: one(registry.providers::<LoopService>())?,
        external_agent: one(registry.providers::<ExternalAgentService>())?,
        model_strategy: one(registry.providers::<ModelStrategyService>())?,
        context_assembler: one(registry.providers::<ContextAssemblerService>())?,
        context_budget: one(registry.providers::<ContextBudgetPolicyService>())?,
        context_reducer: one(registry.providers::<ContextReducerService>())?,
        tool_providers: Some(
            registry
                .providers::<ToolProviderService>()
                .into_iter()
                .map(str::to_owned)
                .collect(),
        ),
        skills: None,
        context_sources: Some(
            registry
                .providers::<ContextContributorService>()
                .into_iter()
                .filter({
                    let mut seen = std::collections::HashSet::new();
                    move |id| seen.insert(*id)
                })
                .map(str::to_owned)
                .collect(),
        ),
        workflow_nodes: Some(
            registry
                .providers::<WorkflowNodeService>()
                .into_iter()
                .map(str::to_owned)
                .collect(),
        ),
        ..AgentSelection::default()
    })
}
pub struct TestStoragePlugin(tempfile::TempDir);
impl Default for TestStoragePlugin {
    fn default() -> Self {
        Self(tempfile::tempdir().expect("test storage directory"))
    }
}
#[async_trait]
impl Plugin for TestStoragePlugin {
    fn id(&self) -> &str {
        "test.sqlite"
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let path = self.0.path().join("agent.db");
        if context.services().get::<StateStoreService>().is_none() {
            let state = Arc::new(
                SqliteStateStore::open(&path, Default::default())
                    .await
                    .map_err(|e| PluginError::message(e.to_string()))?,
            );
            let state_store: Arc<dyn StateStore> = state;
            context.provide(StateStoreService(state_store.clone()))?;
            context.contribute_extension(state_store)?;
        }
        if context
            .services()
            .get::<InteractionStoreService>()
            .is_none()
        {
            let state = Arc::new(
                SqliteStateStore::open(&path, Default::default())
                    .await
                    .map_err(|e| PluginError::message(e.to_string()))?,
            );
            let interaction_store: Arc<dyn InteractionStore> = state;
            context.provide(InteractionStoreService(interaction_store.clone()))?;
            context.contribute_extension(interaction_store)?;
        }
        if context
            .services()
            .get::<ConversationStoreService>()
            .is_none()
        {
            let conversations = Arc::new(
                SqliteConversationStore::open(&path, Default::default())
                    .await
                    .map_err(|e| PluginError::message(e.to_string()))?,
            );
            let conversation_store: Arc<dyn ConversationStore> = conversations;
            context.provide(ConversationStoreService(conversation_store.clone()))?;
            context.contribute_extension(conversation_store)?;
        }
        Ok(())
    }
}

pub mod tool_search;
