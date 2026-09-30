//! Development fixtures only; not a storage option shipped to applications.
use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_storage_local::{SqliteConversationStore, SqliteStateStore};
use std::sync::Arc;
#[allow(dead_code)]
mod conversation;
pub use conversation::*;

pub struct TestBuilder(AgentBuilder);
impl Default for TestBuilder {
    fn default() -> Self {
        Self(AgentBuilder::new())
    }
}
impl TestBuilder {
    pub fn default_models(mut self, enabled: bool) -> Self {
        self.0 = self.0.default_models(enabled);
        self
    }
    pub fn plugin(mut self, plugin: impl Plugin + 'static) -> Self {
        self.0 = self.0.plugin(plugin);
        self
    }
    pub fn plugin_arc(mut self, plugin: Arc<dyn Plugin>) -> Self {
        self.0 = self.0.plugin_arc(plugin);
        self
    }
    pub async fn build(self) -> Result<Agent, AgentError> {
        self.0
            .plugin(TestStoragePlugin(tempfile::tempdir().unwrap()))
            .build()
            .await
    }
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
