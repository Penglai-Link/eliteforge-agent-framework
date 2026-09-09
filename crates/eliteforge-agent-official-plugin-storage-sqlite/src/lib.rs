//! Opt-in default storage bundle. All records persist in one explicitly chosen
//! SQLite file. Framework abstractions have no dependency on SQLite.
use async_trait::async_trait;
use eliteforge_agent_context::{
    ArtifactStore, FileArtifactLimits, SqliteArtifactStore, SqliteSummaryStore, SummaryStore,
    SummaryStoreOptions,
};
use eliteforge_agent_core::*;
use eliteforge_agent_memory::*;
use eliteforge_agent_sqlite_extensions::SqliteExtensionHost;
pub use eliteforge_agent_sqlite_extensions::{
    ExtensionConfig, ExtensionMode, ExtensionReport, ExtensionStatus,
};
use eliteforge_agent_state_sqlite::{SqliteOptions, SqliteStateStore};
pub struct SqliteCapabilitiesService(pub Vec<(String, ExtensionReport)>);
use std::{path::PathBuf, sync::Arc};

pub struct ArtifactStoreService(pub Arc<dyn ArtifactStore>);
pub struct SummaryStoreService(pub Arc<dyn SummaryStore>);
#[derive(Clone)]
pub struct SqliteStoragePlugin {
    path: PathBuf,
    extensions: Option<ExtensionConfig>,
    pub conversations: SqliteConversationOptions,
    pub memory: MemoryStoreOptions,
    pub summaries: SummaryStoreOptions,
    pub execution: SqliteOptions,
    pub artifacts: FileArtifactLimits,
}
impl SqliteStoragePlugin {
    pub fn with_extensions(mut self, config: ExtensionConfig) -> Self {
        self.extensions = Some(config);
        self
    }
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            extensions: None,
            conversations: Default::default(),
            memory: Default::default(),
            summaries: Default::default(),
            execution: Default::default(),
            artifacts: Default::default(),
        }
    }
}
fn install_error(error: impl std::fmt::Display) -> PluginError {
    PluginError::Message(error.to_string())
}
#[async_trait]
impl Plugin for SqliteStoragePlugin {
    fn id(&self) -> &str {
        "storage.sqlite"
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let path = self.path.to_string_lossy();
        if path.is_empty() || path == ":memory:" || path.starts_with("file:") {
            return Err(install_error("SQLite storage requires a filesystem path"));
        }
        let mut reports = Vec::new();
        let state = Arc::new(
            SqliteStateStore::open(&self.path, self.execution.clone())
                .await
                .map_err(install_error)?,
        );
        let close = state.clone();
        context.on_shutdown(move || async move { close.close().await.map_err(|e| e.to_string()) });
        if let Some(config) = &self.extensions {
            reports.push((
                "state".into(),
                state
                    .prepare_extensions(config.clone())
                    .await
                    .map_err(install_error)?,
            ));
        }
        context.provide(StateStoreService(state.clone()))?;
        context.provide(InteractionStoreService(state))?;
        let conversations = Arc::new(
            SqliteConversationStore::open(&self.path, self.conversations.clone())
                .await
                .map_err(install_error)?,
        );
        let close = conversations.clone();
        context.on_shutdown(move || async move { close.close().await.map_err(|e| e.to_string()) });
        if let Some(config) = &self.extensions {
            reports.push((
                "conversations".into(),
                conversations
                    .prepare_extensions(config.clone())
                    .await
                    .map_err(install_error)?,
            ));
        }
        context.provide(ConversationStoreService(conversations))?;
        let memory = Arc::new(
            SqliteLongTermMemory::open(&self.path, self.memory.clone())
                .await
                .map_err(install_error)?,
        );
        let close = memory.clone();
        context.on_shutdown(move || async move { close.close().await.map_err(|e| e.to_string()) });
        if let Some(config) = &self.extensions {
            reports.push((
                "memory".into(),
                memory
                    .prepare_extensions(config.clone())
                    .await
                    .map_err(install_error)?,
            ));
        }
        context.provide(LongTermMemoryService(memory))?;
        let summaries = Arc::new(
            SqliteSummaryStore::open(&self.path, self.summaries.clone())
                .await
                .map_err(install_error)?,
        );
        let close = summaries.clone();
        context.on_shutdown(move || async move { close.close().await.map_err(|e| e.to_string()) });
        if let Some(config) = &self.extensions {
            reports.push((
                "summaries".into(),
                summaries
                    .prepare_extensions(config.clone())
                    .await
                    .map_err(install_error)?,
            ));
        }
        context.provide(SummaryStoreService(summaries))?;
        let artifacts = Arc::new(
            SqliteArtifactStore::open(&self.path, self.artifacts)
                .await
                .map_err(install_error)?,
        );
        let close = artifacts.clone();
        context.on_shutdown(move || async move { close.close().await.map_err(|e| e.to_string()) });
        if let Some(config) = &self.extensions {
            reports.push((
                "artifacts".into(),
                artifacts
                    .prepare_extensions(config.clone())
                    .await
                    .map_err(install_error)?,
            ));
        }
        context.provide(ArtifactStoreService(artifacts))?;
        context.provide(SqliteCapabilitiesService(reports))
    }
}
