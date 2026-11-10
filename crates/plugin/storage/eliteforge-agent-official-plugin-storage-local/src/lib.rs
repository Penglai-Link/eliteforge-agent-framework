//! Local-first storage for Framework agents. SQLite owns structured records;
//! a host-protected directory owns artifact bytes.
pub const STORAGE_IMPLEMENTATION_DESCRIPTION: &str =
    "Local-first storage: SQLite for structured records and the filesystem for artifacts.";

mod artifact;
mod conversation;
mod database;
mod extensions;
mod history;
mod lifecycle;
mod local_artifact;
mod long_term;
mod model_catalog;
mod operations;
mod runtime;
mod state;
mod summary;
mod telemetry;
pub use telemetry::{SqliteTelemetryStore, TelemetryStoragePlugin};

pub use artifact::{FileArtifactLimits, FileArtifactStore};
pub use conversation::{SqliteConversationOptions, SqliteConversationStore};
pub use database::SqliteDatabase;
pub use extensions::{
    ExtensionConfig, ExtensionError, ExtensionMode, ExtensionReport, ExtensionStatus, Platform,
    SqliteExtensionHost,
};
pub use lifecycle::SqliteLifecycleStore;
pub use local_artifact::LocalArtifactStore;
pub use long_term::{MemoryStoreOptions, SqliteLongTermMemory};
pub use model_catalog::SqliteModelCatalogStore;
pub use operations::*;
pub use runtime::{BorrowedLocalStoragePlugin, LocalStorageRuntime};
pub use state::{SqliteExecutionPlugin, SqliteOptions, SqliteStateStore};
pub use summary::{SqliteSummaryStore, SummaryStoreOptions};

use async_trait::async_trait;
pub use eliteforge_agent_context_contracts::{
    ArtifactChunk, ArtifactError, ArtifactId, ArtifactMetadata, ArtifactStore, ArtifactStorePlugin,
    ArtifactStoreService, ConversationSummary, SummaryKey, SummaryStore, SummaryStorePlugin,
    SummaryStoreService, VersionedSummary,
};
use eliteforge_agent_core::*;
pub use eliteforge_agent_core::{
    DeleteSession, DeleteSessions, SessionDeletionFailure, SessionDeletionReport, SessionStore,
    SessionStorePlugin, SessionStoreService,
};
use std::{path::PathBuf, sync::Arc};

pub struct SqliteCapabilitiesService(pub Vec<(String, ExtensionReport)>);

#[derive(Clone)]
pub struct LocalStorageConfig {
    pub database_path: PathBuf,
    pub database: Option<SqliteDatabase>,
    /// The host creates and protects this directory before plugin installation.
    pub artifact_root: PathBuf,
    pub extensions: Option<ExtensionConfig>,
    pub conversations: SqliteConversationOptions,
    pub memory: MemoryStoreOptions,
    pub summaries: SummaryStoreOptions,
    pub execution: SqliteOptions,
    pub artifacts: FileArtifactLimits,
}

impl LocalStorageConfig {
    pub fn new(database_path: impl Into<PathBuf>, artifact_root: impl Into<PathBuf>) -> Self {
        Self {
            database_path: database_path.into(),
            database: None,
            artifact_root: artifact_root.into(),
            extensions: None,
            conversations: Default::default(),
            memory: Default::default(),
            summaries: Default::default(),
            execution: Default::default(),
            artifacts: Default::default(),
        }
    }
}

#[derive(Clone)]
pub struct LocalStoragePlugin {
    pub config: LocalStorageConfig,
}

impl LocalStoragePlugin {
    pub fn new(database_path: impl Into<PathBuf>, artifact_root: impl Into<PathBuf>) -> Self {
        Self {
            config: LocalStorageConfig::new(database_path, artifact_root),
        }
    }

    pub fn with_database(mut self, database: SqliteDatabase) -> Self {
        self.config.database = Some(database);
        self
    }

    pub fn with_config(config: LocalStorageConfig) -> Self {
        Self { config }
    }

    pub fn with_extensions(mut self, extensions: ExtensionConfig) -> Self {
        self.config.extensions = Some(extensions);
        self
    }
}

fn install_error(error: impl std::fmt::Display) -> PluginError {
    PluginError::Message(error.to_string())
}

#[async_trait]
impl Plugin for LocalStoragePlugin {
    fn id(&self) -> &str {
        "storage.local"
    }

    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let config = &self.config;
        let path = config.database_path.to_string_lossy();
        if path.is_empty() || path == ":memory:" || path.starts_with("file:") {
            return Err(install_error(
                "local storage requires a SQLite filesystem path",
            ));
        }
        if config.artifact_root.as_os_str().is_empty() {
            return Err(install_error(
                "local storage requires an artifact directory",
            ));
        }

        // Check the directory capability first: a missing directory is an
        // installation error, and this plugin never creates one implicitly.
        let artifact_files = FileArtifactStore::open(&config.artifact_root, config.artifacts)
            .map_err(install_error)?;
        let database = match &config.database {
            Some(database) => database.clone(),
            None => SqliteDatabase::open(&config.database_path)
                .await
                .map_err(install_error)?,
        };
        if config.database.is_none() {
            let close = database.clone();
            context
                .on_shutdown(move || async move { close.close().await.map_err(|e| e.to_string()) });
        }
        context.provide(TransactionManagerService(Arc::new(database.clone())))?;
        context.contribute_extension(Arc::new(database.clone()) as Arc<dyn TransactionManager>)?;
        context.provide(database.clone())?;
        let lifecycle = Arc::new(
            SqliteLifecycleStore::with_database(database.clone())
                .await
                .map_err(install_error)?,
        );
        lifecycle
            .recover_expired_runs()
            .await
            .map_err(install_error)?;
        context.provide(SessionLifecycleStoreService(lifecycle.clone()))?;
        let close = lifecycle.clone();
        context.on_shutdown(move || async move {
            close.close();
            Ok(())
        });
        context.contribute_extension(lifecycle as Arc<dyn SessionLifecycleStore>)?;
        let mut reports = Vec::new();

        let state = Arc::new(
            SqliteStateStore::with_database(database.clone(), config.execution.clone())
                .await
                .map_err(install_error)?
                .with_artifact_files(Arc::new(artifact_files.clone())),
        );
        state.cleanup_artifacts().await;
        let close = state.clone();
        context.on_shutdown(move || async move { close.close().await.map_err(|e| e.to_string()) });
        if let Some(extensions) = &config.extensions {
            reports.push((
                "state".into(),
                state
                    .prepare_extensions(extensions.clone())
                    .await
                    .map_err(install_error)?,
            ));
        }
        context.provide(StateStoreService(state.clone()))?;
        context.contribute_extension(state.clone() as Arc<dyn StateStore>)?;
        context.provide(InteractionStoreService(state.clone()))?;
        context.contribute_extension(state.clone() as Arc<dyn InteractionStore>)?;
        context.provide(SessionStoreService(state.clone()))?;
        context.provide(HistoryStoreService(state.clone()))?;
        context.contribute_extension(state as Arc<dyn SessionStore>)?;

        let conversations = Arc::new(
            SqliteConversationStore::with_database(database.clone(), config.conversations.clone())
                .await
                .map_err(install_error)?,
        );
        let close = conversations.clone();
        context.on_shutdown(move || async move { close.close().await.map_err(|e| e.to_string()) });
        if let Some(extensions) = &config.extensions {
            reports.push((
                "conversations".into(),
                conversations
                    .prepare_extensions(extensions.clone())
                    .await
                    .map_err(install_error)?,
            ));
        }
        context.provide(ConversationStoreService(conversations.clone()))?;
        context.contribute_extension(conversations as Arc<dyn ConversationStore>)?;

        let memory = Arc::new(
            SqliteLongTermMemory::with_database(database.clone(), config.memory.clone())
                .await
                .map_err(install_error)?,
        );
        let close = memory.clone();
        context.on_shutdown(move || async move { close.close().await.map_err(|e| e.to_string()) });
        if let Some(extensions) = &config.extensions {
            reports.push((
                "memory".into(),
                memory
                    .prepare_extensions(extensions.clone())
                    .await
                    .map_err(install_error)?,
            ));
        }
        context.provide(LongTermMemoryService(memory.clone()))?;
        context.contribute_extension(memory as Arc<dyn LongTermMemory>)?;

        let summaries = Arc::new(
            SqliteSummaryStore::with_database(database.clone(), config.summaries.clone())
                .await
                .map_err(install_error)?,
        );
        let close = summaries.clone();
        context.on_shutdown(move || async move { close.close().await.map_err(|e| e.to_string()) });
        if let Some(extensions) = &config.extensions {
            reports.push((
                "summaries".into(),
                summaries
                    .prepare_extensions(extensions.clone())
                    .await
                    .map_err(install_error)?,
            ));
        }
        context.provide(SummaryStoreService(summaries.clone()))?;
        context.contribute_extension(summaries as Arc<dyn SummaryStore>)?;

        let artifacts = Arc::new(
            LocalArtifactStore::with_database(database.clone(), artifact_files)
                .await
                .map_err(install_error)?,
        );
        let close = artifacts.clone();
        context.on_shutdown(move || async move { close.close().await.map_err(|e| e.to_string()) });
        if let Some(extensions) = &config.extensions {
            reports.push((
                "artifacts".into(),
                artifacts
                    .prepare_extensions(extensions.clone())
                    .await
                    .map_err(install_error)?,
            ));
        }
        context.provide(ArtifactStoreService(artifacts.clone()))?;
        context.contribute_extension(artifacts as Arc<dyn ArtifactStore>)?;

        let catalog = Arc::new(
            SqliteModelCatalogStore::with_database(database.clone())
                .await
                .map_err(install_error)?,
        );
        let close = catalog.clone();
        context.on_shutdown(move || async move { close.close().await.map_err(|e| e.to_string()) });
        if let Some(extensions) = &config.extensions {
            reports.push((
                "model_catalog".into(),
                catalog
                    .prepare_extensions(extensions.clone())
                    .await
                    .map_err(install_error)?,
            ));
        }
        context.provide(ModelCatalogStoreService(catalog.clone()))?;
        context.contribute_extension(catalog as Arc<dyn ModelCatalogStore>)?;
        context.provide(SqliteCapabilitiesService(reports))
    }
}
