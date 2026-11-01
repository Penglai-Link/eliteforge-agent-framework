//! Optional PersonAI integration. Account credentials remain owned by the host.

#[cfg(test)]
use eliteforge_agent_test_support::TestBuilder;
mod auto_routing;
mod connection;
mod context;
mod document_parsing;
mod embedding_candidates;
mod models;
mod realtime;
mod services;
mod sync;
mod tools;
pub use auto_routing::*;
pub use document_parsing::{PersonAiDocumentParsingIntent, personai_document_parse_mode};
pub use embedding_candidates::{
    PersonAiEmbeddingKind, PersonAiEmbeddingModel, personai_embedding_batch_limit,
    personai_embedding_candidates,
};

pub use connection::PersonAiConnection;
pub use context::{AccountContext, PersonAiContextHandle, PersonAiContextProvider};
pub use crab_serverapi as sdk;
pub use models::{
    PersonAiModels, personai_embedding_request_body, personai_generation_request_body,
};
pub use services::{PersonAiServices, detect_device_id, validate_device_id};
pub use sync::{PersonAiModelSync, PersonAiSyncStatus, SyncDomainStatus};
pub use tools::{
    PersonAiToolProvider, personai_tool_catalog, personai_tool_catalog_with_categories,
};

use async_trait::async_trait;
use eliteforge_agent_core::{
    CapabilityConfig, CapabilityPack, Plugin, PluginContext, PluginError, PluginReadyContext,
};
use eliteforge_agent_framework::{
    ModelCatalogRefreshService, ModelRuntime, ModelRuntimeService, ModelRuntimeUpdate,
};
use std::{sync::Arc, time::Duration};

/// Settings for an optional PersonAI connection. The host supplies live credentials.
#[derive(Clone)]
pub struct PersonAiConfig {
    pub server_base_url: String,
    pub connection_id: String,
    pub sync_interval: Duration,
    /// Responses-only models are absent from PersonAI's Chat model-list endpoint.
    pub responses_only_models: Vec<String>,
    /// Text embedding bindings are explicitly supplied by the host.
    pub embedding_models: Vec<String>,
    /// Host-verified models that accept text and images in the same embedding space.
    /// The public media catalog does not declare input modalities; never infer them from names.
    pub multimodal_embedding_models: Vec<String>,
}

impl PersonAiConfig {
    pub fn new(server_base_url: impl Into<String>) -> Self {
        Self {
            server_base_url: server_base_url.into(),
            connection_id: "personai".into(),
            sync_interval: Duration::from_secs(300),
            responses_only_models: Vec::new(),
            embedding_models: Vec::new(),
            multimodal_embedding_models: Vec::new(),
        }
    }
}

/// Attaches an initialized host connection without starting another synchronizer.
pub struct PersonAiCapabilityPack {
    connection: Arc<PersonAiConnection>,
    register_tools: bool,
    auto_routers: Option<eliteforge_agent_core::AutoRouters>,
}

impl PersonAiCapabilityPack {
    pub fn new(connection: Arc<PersonAiConnection>) -> Self {
        Self {
            connection,
            register_tools: true,
            auto_routers: None,
        }
    }

    /// Raw provider for composition with ToolDiscoveryPlugin. Install this provider
    /// through discovery and call `without_tools` to avoid duplicate registration.
    pub fn tools(&self) -> PersonAiToolProvider {
        PersonAiToolProvider::new(self.connection.clone())
    }

    /// Keep SDK services and model adapters, while the host registers `tools()`
    /// through its own shared discovery catalog or other ToolProvider composition.
    pub fn without_tools(mut self) -> Self {
        self.register_tools = false;
        self
    }
    /// Install selected routing implementations with the host's data source.
    pub fn with_auto_routers(mut self, routers: eliteforge_agent_core::AutoRouters) -> Self {
        self.auto_routers = Some(routers);
        self
    }

    /// Host access before this pack is moved into `FrameworkBuilder::capability_pack`.
    pub fn services(&self) -> Arc<PersonAiServices> {
        self.connection.services()
    }
    pub fn sync(&self) -> Arc<PersonAiModelSync> {
        self.connection.sync()
    }
}

impl CapabilityPack for PersonAiCapabilityPack {
    fn id(&self) -> &str {
        "framework.personai"
    }
    fn configuration(&self) -> &CapabilityConfig {
        &self.connection.config
    }
    fn plugins(&self) -> Vec<Arc<dyn Plugin>> {
        let mut plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(PersonAiPlugin {
            connection: self.connection.clone(),
            runtime: ModelRuntime::new(ModelRuntimeUpdate::new("", 0, vec![]))
                .expect("empty model runtime is valid"),
        })];
        if let Some(routers) = &self.auto_routers {
            plugins.push(Arc::new(eliteforge_agent_core::AutoRoutersPlugin(
                routers.clone(),
            )));
        }
        if self.register_tools {
            plugins.push(Arc::new(
                eliteforge_agent_official_plugin_tools_discovery::ToolDiscoveryPlugin::new()
                    .provider(self.tools()),
            ));
        }
        plugins
    }
}

struct PersonAiPlugin {
    connection: Arc<PersonAiConnection>,
    runtime: Arc<ModelRuntime>,
}

#[async_trait]
impl Plugin for PersonAiPlugin {
    fn id(&self) -> &str {
        "capability.personai"
    }

    async fn install(&self, plugin: &mut PluginContext<'_>) -> Result<(), PluginError> {
        if self.connection.is_closed() {
            return Err(PluginError::message("PersonAI connection is closed"));
        }
        plugin.provide(self.connection.clone())?;
        plugin.provide(self.connection.services.clone())?;
        plugin.provide(self.connection.models.clone())?;
        plugin.provide(self.connection.sync.clone())?;
        plugin.contribute(ModelCatalogRefreshService(self.connection.sync.clone()))?;
        plugin.provide(ModelRuntimeService(self.runtime.clone()))?;
        Ok(())
    }

    async fn ready(&self, plugin: &mut PluginReadyContext<'_>) -> Result<(), PluginError> {
        if self.connection.is_closed() {
            return Err(PluginError::message("PersonAI connection is closed"));
        }
        self.connection
            .sync
            .attach_runtime(&self.runtime)
            .map_err(PluginError::message)?;
        let sync = self.connection.sync.clone();
        let runtime = self.runtime.clone();
        plugin.on_cleanup(move || sync.detach_runtime(&runtime));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eliteforge_agent_core::ToolProvider;
    use eliteforge_agent_plugin_core::PluginContainer;

    struct LocalEcho;
    #[async_trait]
    impl eliteforge_agent_core::ChatModel for LocalEcho {
        async fn generate(
            &self,
            model: &str,
            _: eliteforge_agent_core::ChatRequest,
            _: &dyn eliteforge_agent_core::ChatStream,
        ) -> Result<eliteforge_agent_core::ChatResponse, eliteforge_agent_core::ModelError>
        {
            Ok(eliteforge_agent_core::ChatResponse::text(model))
        }
    }

    #[tokio::test]
    async fn shared_connection_builds_agents_with_independent_models_and_selection() {
        use eliteforge_agent_framework::*;
        let server = sync::tests::MockDiscovery::start().await;
        let context = Arc::new(PersonAiContextHandle::default());
        context.publish(AccountContext {
            account_scope: "alice".into(),
            revision: 1,
            authorization: sdk::base::AuthorizedClientContext::new()
                .with_token("test")
                .with_product_code("test")
                .with_product_version("1.0.0"),
        });
        let mut config = PersonAiConfig::new(&server.url);
        config
            .responses_only_models
            .push("explicit-responses".into());
        let connection = PersonAiConnection::connect(config, context.clone(), None)
            .await
            .unwrap();
        let mut agents = Vec::new();
        for model in ["agent-a", "agent-b"] {
            agents.push(
                TestBuilder::new()
                    .default_models(false)
                    .capability_pack(PersonAiCapabilityPack::new(connection.clone()))
                    .plugin(eliteforge_agent_test_support::TestStoragePlugin::default())
                    .plugin(ModelConnectionPlugin::new(ModelConnection::new(
                        "local",
                        Arc::new(LocalEcho),
                        vec![ModelDescriptor::new(model)],
                    )))
                    .plugin(ModelStrategyPlugin(Arc::new(FixedModelStrategy(
                        ModelRef::new("local", model),
                    ))))
                    .plugin(ModelRoutingPlugin(
                        ModelRouting::default()
                            .default_for(
                                ModelOperation::VideoGeneration,
                                ModelRef::new("personai", "v1"),
                            )
                            .stage_for(
                                ModelOperation::VideoGeneration,
                                "render",
                                ModelRef::new("personai", "v1"),
                            ),
                    ))
                    .build()
                    .await
                    .unwrap(),
            );
        }
        // Explicit Responses-only configuration is available during build validation.
        let explicit = TestBuilder::new()
            .default_models(false)
            .capability_pack(PersonAiCapabilityPack::new(connection.clone()))
            .plugin(eliteforge_agent_test_support::TestStoragePlugin::default())
            .plugin(ModelStrategyPlugin(Arc::new(FixedModelStrategy(
                ModelRef::new("personai", "explicit-responses"),
            ))))
            .build()
            .await
            .unwrap();
        assert_eq!(
            server.requests.load(std::sync::atomic::Ordering::SeqCst),
            10
        );
        // A shared publication must reach every attached Agent even when their
        // default/stage routing references models removed by discovery.
        *server.video.lock().unwrap() = "[]".into();
        connection.sync().refresh_now().await.unwrap();
        for agent in &agents {
            assert!(agent.models(ModelOperation::VideoGeneration).is_empty());
            for stage in ["", "render"] {
                assert!(matches!(
                    agent
                        .submit_video(VideoGenerationRequest {
                            choice: ModelChoice {
                                model: None,
                                stage: stage.into()
                            },
                            prompt: "clip".into(),
                            seconds: 2,
                            size: None,
                            first_frame: None,
                            last_frame: None,
                            reference_images: vec![],
                            reference_videos: vec![],
                            reference_audios: vec![],
                            provider_options: Default::default(),
                        })
                        .await,
                    Err(AgentError::Model(ModelError::Unavailable(_)))
                ));
            }
        }
        for (agent, own, other) in [
            (&agents[0], "agent-a", "agent-b"),
            (&agents[1], "agent-b", "agent-a"),
        ] {
            let models = agent.models(ModelOperation::Chat);
            assert!(models.contains(&ModelRef::new("local", own)));
            assert!(!models.contains(&ModelRef::new("local", other)));
            assert!(models.contains(&ModelRef::new("personai", "explicit-responses")));
            assert_eq!(
                agent
                    .run(RunRequest::new(own, "hello"))
                    .await
                    .unwrap()
                    .output,
                own
            );
        }
        agents[0].close().await;
        assert!(!connection.is_closed());
        *server.chat_low.lock().unwrap() = "[]".into();
        connection.sync().refresh_now().await.unwrap();
        // No yield or polling: completed refresh is visible to attached Agents.
        assert!(
            !agents[1]
                .models(ModelOperation::Chat)
                .contains(&ModelRef::new("personai", "p/old"))
        );
        assert!(
            agents[1]
                .models(ModelOperation::Chat)
                .contains(&ModelRef::new("local", "agent-b"))
        );
        assert_eq!(
            agents[1]
                .run(RunRequest::new("after-close", "hello"))
                .await
                .unwrap()
                .output,
            "agent-b"
        );
        context.logout();
        connection.sync().refresh_now().await.unwrap();
        assert_eq!(
            agents[1].models(ModelOperation::Chat),
            vec![ModelRef::new("local", "agent-b")]
        );
        assert!(explicit.models(ModelOperation::Chat).is_empty());
        agents[1].close().await;
        explicit.close().await;
        connection.close().await.unwrap();
    }

    #[tokio::test]
    async fn pack_exposes_sdk_services_and_agent_tools() {
        let context = Arc::new(PersonAiContextHandle::default());
        context.publish(AccountContext {
            account_scope: "test".into(),
            revision: 1,
            authorization: sdk::base::AuthorizedClientContext::new()
                .with_token("test")
                .with_product_code("test")
                .with_product_version("1.0.0"),
        });
        let server = sync::tests::MockDiscovery::start().await;
        let connection =
            PersonAiConnection::connect(PersonAiConfig::new(&server.url), context, None)
                .await
                .unwrap();
        assert_eq!(
            server.requests.load(std::sync::atomic::Ordering::SeqCst),
            10
        );
        let pack = PersonAiCapabilityPack::new(connection.clone());
        let host_services = pack.services();
        let host_sync = pack.sync();
        let mut container = PluginContainer::build(pack.plugins()).await.unwrap();
        let mut second = PluginContainer::build(pack.plugins()).await.unwrap();
        let first_runtime = container
            .services()
            .get::<ModelRuntimeService>()
            .unwrap()
            .0
            .clone();
        let second_runtime = second
            .services()
            .get::<ModelRuntimeService>()
            .unwrap()
            .0
            .clone();
        assert!(!Arc::ptr_eq(&first_runtime, &second_runtime));
        assert_eq!(first_runtime.snapshot().chat.models().len(), 1);
        assert_eq!(second_runtime.snapshot().chat.models().len(), 1);
        assert_eq!(
            server.requests.load(std::sync::atomic::Ordering::SeqCst),
            10
        );
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
        assert_eq!(
            container.services().extensions::<dyn ToolProvider>().len(),
            1
        );
        let mut services_only = PluginContainer::build(
            PersonAiCapabilityPack::new(connection.clone())
                .without_tools()
                .plugins(),
        )
        .await
        .unwrap();
        assert!(
            services_only
                .services()
                .extensions::<dyn ToolProvider>()
                .is_empty()
        );
        services_only.close_async(Duration::from_secs(1)).await;
        container.close_async(Duration::from_secs(1)).await;
        assert!(!connection.is_closed());
        assert_eq!(second_runtime.snapshot().chat.models().len(), 1);
        connection.sync().refresh_now().await.unwrap();
        connection.close().await.unwrap();
        assert!(connection.models.bind_current().is_err());
        tokio::time::timeout(Duration::from_secs(1), async {
            while !second_runtime.snapshot().chat.models().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        second.close_async(Duration::from_secs(1)).await;
    }

    #[tokio::test]
    async fn initial_discovery_failure_does_not_publish_a_connection() {
        let server = sync::tests::MockDiscovery::start().await;
        *server.chat_low.lock().unwrap() = "null".into();
        let context = Arc::new(PersonAiContextHandle::default());
        context.publish(AccountContext {
            account_scope: "test".into(),
            revision: 1,
            authorization: sdk::base::AuthorizedClientContext::new()
                .with_token("test")
                .with_product_code("test")
                .with_product_version("1.0.0"),
        });
        let result =
            PersonAiConnection::connect(PersonAiConfig::new(&server.url), context, None).await;
        assert!(matches!(result, Err(error) if error.contains("chat")));
        assert!(server.requests.load(std::sync::atomic::Ordering::SeqCst) > 0);
    }
}
