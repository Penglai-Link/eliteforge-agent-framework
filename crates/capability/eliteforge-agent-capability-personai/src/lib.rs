//! Optional PersonAI integration. Account credentials remain owned by the host.

mod context;
mod models;
mod realtime;
mod services;
mod sync;

pub use context::{AccountContext, PersonAiContextHandle, PersonAiContextProvider};
pub use models::PersonAiModels;
pub use services::PersonAiServices;
pub use sync::{PersonAiModelSync, PersonAiSyncStatus, SyncDomainStatus};

use async_trait::async_trait;
use eliteforge_agent_core::{
    ModelCatalogStore, Plugin, PluginContext, PluginError, PluginReadyContext,
};
use eliteforge_agent_framework::{ModelRuntime, ModelRuntimeService, ModelRuntimeUpdate};
use std::{sync::Arc, time::Duration};

/// Settings for an optional PersonAI connection. The host supplies live credentials.
#[derive(Clone)]
pub struct PersonAiConfig {
    pub server_base_url: String,
    pub connection_id: String,
    pub sync_interval: Duration,
    /// Responses-only models are absent from PersonAI's Chat model-list endpoint.
    pub responses_only_models: Vec<String>,
    /// Embedding models have no discovery endpoint and must be configured explicitly.
    pub embedding_models: Vec<String>,
}

impl PersonAiConfig {
    pub fn new(server_base_url: impl Into<String>) -> Self {
        Self {
            server_base_url: server_base_url.into(),
            connection_id: "personai".into(),
            sync_interval: Duration::from_secs(300),
            responses_only_models: Vec::new(),
            embedding_models: Vec::new(),
        }
    }
}

/// Installs typed SDK services, model adapters, and the background catalog sync.
pub struct PersonAiCapabilityPack {
    config: PersonAiConfig,
    context: Arc<dyn PersonAiContextProvider>,
}

impl PersonAiCapabilityPack {
    pub fn new(config: PersonAiConfig, context: Arc<dyn PersonAiContextProvider>) -> Self {
        Self { config, context }
    }
}

#[async_trait]
impl Plugin for PersonAiCapabilityPack {
    fn id(&self) -> &str {
        "capability.personai"
    }

    async fn install(&self, plugin: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let services = Arc::new(
            PersonAiServices::new(&self.config.server_base_url)
                .map_err(|error| PluginError::message(error.to_string()))?,
        );
        let models = Arc::new(PersonAiModels::new(
            self.config.connection_id.clone(),
            services.clone(),
            self.context.clone(),
        ));
        let runtime = ModelRuntime::new(ModelRuntimeUpdate::new("", 0, vec![]))
            .map_err(|error| PluginError::message(error.to_string()))?;
        let sync = Arc::new(PersonAiModelSync::new(
            self.config.clone(),
            services.clone(),
            self.context.clone(),
            models.clone(),
            runtime.clone(),
        ));
        plugin.provide(services)?;
        plugin.provide(models)?;
        plugin.provide(sync.clone())?;
        plugin.provide(ModelRuntimeService(runtime))?;
        Ok(())
    }

    async fn ready(&self, plugin: &mut PluginReadyContext<'_>) -> Result<(), PluginError> {
        let sync = plugin
            .services()
            .get::<Arc<PersonAiModelSync>>()
            .ok_or_else(|| PluginError::message("PersonAI sync service missing"))?;
        sync.set_store(plugin.extension::<dyn ModelCatalogStore>())
            .await;
        let stop = tokio_util::sync::CancellationToken::new();
        plugin.on_cleanup({
            let stop = stop.clone();
            move || stop.cancel()
        });
        let handle = sync.start(stop.clone());
        plugin.on_shutdown(move || async move {
            stop.cancel();
            handle.await.map_err(|error| error.to_string())
        });
        Ok(())
    }
}
