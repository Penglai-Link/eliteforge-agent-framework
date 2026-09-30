//! Optional PersonAI integration. Account credentials remain owned by the host.

mod context;
mod models;
mod realtime;
mod services;
mod sync;

pub use context::{AccountContext, PersonAiContextHandle, PersonAiContextProvider};
pub use crab_serverapi as sdk;
pub use models::PersonAiModels;
pub use services::{PersonAiServices, detect_device_id, validate_device_id};
pub use sync::{PersonAiModelSync, PersonAiSyncStatus, SyncDomainStatus};

use async_trait::async_trait;
use eliteforge_agent_core::{
    CapabilityConfig, CapabilityPack, ModelCatalogStore, Plugin, PluginContext, PluginError,
    PluginReadyContext,
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
    plugin: Arc<PersonAiPlugin>,
    services: Arc<PersonAiServices>,
    sync: Arc<PersonAiModelSync>,
}

impl PersonAiCapabilityPack {
    pub fn new(
        config: PersonAiConfig,
        context: Arc<dyn PersonAiContextProvider>,
    ) -> Result<Self, String> {
        let services = Arc::new(
            PersonAiServices::new(&config.server_base_url).map_err(|error| error.to_string())?,
        );
        let models = Arc::new(
            PersonAiModels::new(
                config.connection_id.clone(),
                &config.server_base_url,
                services.clone(),
                context.clone(),
            )
            .map_err(|error| error.to_string())?,
        );
        let runtime = ModelRuntime::new(ModelRuntimeUpdate::new("", 0, vec![]))
            .map_err(|error| error.to_string())?;
        let sync = Arc::new(PersonAiModelSync::new(
            config.clone(),
            services.clone(),
            context,
            models.clone(),
            runtime.clone(),
        ));
        let plugin = Arc::new(PersonAiPlugin {
            services: services.clone(),
            models,
            runtime,
            sync: sync.clone(),
        });
        Ok(Self {
            config,
            plugin,
            services,
            sync,
        })
    }

    /// Host access before this pack is moved into `AgentBuilder::capability_pack`.
    pub fn services(&self) -> Arc<PersonAiServices> {
        self.services.clone()
    }
    pub fn sync(&self) -> Arc<PersonAiModelSync> {
        self.sync.clone()
    }
}

impl CapabilityPack for PersonAiCapabilityPack {
    fn id(&self) -> &str {
        "framework.personai"
    }
    fn configuration(&self) -> &CapabilityConfig {
        &self.config
    }
    fn plugins(&self) -> Vec<Arc<dyn Plugin>> {
        vec![self.plugin.clone()]
    }
    fn rebuildable(&self) -> bool {
        false
    }
}

struct PersonAiPlugin {
    services: Arc<PersonAiServices>,
    models: Arc<PersonAiModels>,
    runtime: Arc<ModelRuntime>,
    sync: Arc<PersonAiModelSync>,
}

#[async_trait]
impl Plugin for PersonAiPlugin {
    fn id(&self) -> &str {
        "capability.personai"
    }

    async fn install(&self, plugin: &mut PluginContext<'_>) -> Result<(), PluginError> {
        plugin.provide(self.services.clone())?;
        plugin.provide(self.models.clone())?;
        plugin.provide(self.sync.clone())?;
        plugin.provide(ModelRuntimeService(self.runtime.clone()))?;
        Ok(())
    }

    async fn ready(&self, plugin: &mut PluginReadyContext<'_>) -> Result<(), PluginError> {
        self.sync
            .set_store(plugin.extension::<dyn ModelCatalogStore>())
            .await;
        let stop = tokio_util::sync::CancellationToken::new();
        plugin.on_cleanup({
            let stop = stop.clone();
            move || stop.cancel()
        });
        let handle = self.sync.start(stop.clone());
        plugin.on_shutdown(move || async move {
            stop.cancel();
            handle.await.map_err(|error| error.to_string())
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eliteforge_agent_core::ToolProvider;
    use eliteforge_agent_plugin_core::PluginContainer;

    #[tokio::test]
    async fn pack_exposes_sdk_services_without_agent_business_tools() {
        let context = Arc::new(PersonAiContextHandle::default());
        let pack = PersonAiCapabilityPack::new(PersonAiConfig::new("http://127.0.0.1:1"), context)
            .unwrap();
        let host_services = pack.services();
        let host_sync = pack.sync();
        let mut container = PluginContainer::build(pack.plugins()).await.unwrap();
        assert!(Arc::ptr_eq(
            &host_services,
            container
                .services()
                .get::<Arc<PersonAiServices>>()
                .unwrap()
                .as_ref()
        ));
        assert!(Arc::ptr_eq(
            &host_sync,
            container
                .services()
                .get::<Arc<PersonAiModelSync>>()
                .unwrap()
                .as_ref()
        ));
        let _ = &host_services.login;
        let _ = &host_services.account;
        let _ = &host_services.checkin;
        let _ = &host_services.chat;
        let _ = &host_services.responses;
        let _ = &host_services.embeddings;
        let _ = &host_services.search;
        let _ = &host_services.multimedia;
        let _ = &host_services.audio_file;
        let _ = &host_services.audio_realtime;
        let _ = &host_services.skills;
        let _ = &host_services.roles;
        let _ = &host_services.repositories;
        let _ = &host_services.sites;
        let _ = &host_services.skins;
        let _ = &host_services.files;
        let _ = &host_services.copyright;
        let _ = &host_services.notifications;
        let _ = &host_services.ads;
        let _ = &host_services.feedback;
        let _ = &host_services.versions;
        let _ = &host_services.relay;
        assert!(container.services().get::<ModelRuntimeService>().is_some());
        assert!(
            container
                .services()
                .extensions::<dyn ToolProvider>()
                .is_empty()
        );
        container.close_async(Duration::from_secs(1)).await;
    }
}
