//! Development fixtures only; not a storage option shipped to applications.
use agent_framework::*;
use agent_memory::*;
use agent_state_sqlite::*;
use async_trait::async_trait;
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
            context.provide(StateStoreService(state))?;
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
            context.provide(InteractionStoreService(state))?;
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
            context.provide(ConversationStoreService(conversations))?;
        }
        Ok(())
    }
}
