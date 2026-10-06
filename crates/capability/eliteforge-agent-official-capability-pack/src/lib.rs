//! Official plugin configuration in one FrameworkBuilder::capability_pack call.
//!
//! Defaults include Genai, local storage, human interaction, agent collaboration,
//! Skills, AG-UI, MCP and telemetry. Local filesystem/command packs are opt-in.
//! Plugins run in the host process; only configured external services or commands
//! create child processes. Resource acquisition and shutdown belong to the Agent.

use eliteforge_agent_core::{CapabilityConfig, CapabilityPack, ConfiguredPluginFactory, Plugin};
use std::{path::PathBuf, sync::Arc};

pub use eliteforge_agent_capability_tools as tools;
pub use eliteforge_agent_official_plugin_agui as agui;
pub use eliteforge_agent_official_plugin_model_genai as model;
pub use eliteforge_agent_official_plugin_otel as telemetry;
pub use eliteforge_agent_official_plugin_skills as skills;
pub use eliteforge_agent_official_plugin_storage_local as storage;
pub use eliteforge_agent_official_plugin_tools_agents as agents;
pub use eliteforge_agent_official_plugin_tools_human as human;
pub use eliteforge_agent_official_plugin_tools_mcp as mcp;

#[cfg(feature = "discovery")]
pub use eliteforge_agent_official_plugin_tools_discovery as discovery;
#[cfg(feature = "media")]
pub use eliteforge_agent_official_plugin_tools_media as media;
#[cfg(feature = "web")]
pub use eliteforge_agent_official_plugin_tools_web as web;

/// Official defaults and opt-in local tools with an explicit database path.
///
/// Configure replacements on the pack itself. Adding a plugin with an existing
/// ID through FrameworkBuilder::plugin is an error, not an override.
pub struct EliteForgeOfficialCapabilityPack {
    model: Option<Arc<dyn Plugin>>,
    storage: Arc<storage::LocalStoragePlugin>,
    model_factory: Option<ConfiguredPluginFactory>,
    human_interaction: bool,
    agent_tools: bool,
    skills: Option<Arc<skills::SkillsPlugin>>,
    agui: Option<Arc<agui::AgUiPlugin>>,
    mcp: Vec<Arc<mcp::McpToolsPlugin>>,
    telemetry: Vec<Arc<telemetry::TelemetryPlugin>>,
    read_only_filesystem: Option<tools::FileSystemToolsConfig>,
    write_filesystem: Option<tools::FileSystemToolsConfig>,
    system_commands: Option<tools::SystemCommandConfig>,
    #[cfg(feature = "web")]
    web: Option<Arc<web::WebToolsPlugin>>,
    #[cfg(feature = "media")]
    media: Option<media::MediaToolsConfig>,
    #[cfg(feature = "discovery")]
    discovery: Option<Arc<discovery::ToolDiscoveryPlugin>>,
}

impl EliteForgeOfficialCapabilityPack {
    /// Register the default in-process capabilities. MCP starts with no servers; telemetry
    /// reads the host's global providers during installation.
    pub fn new(database: impl Into<PathBuf>, artifact_root: impl Into<PathBuf>) -> Self {
        Self {
            model: None,
            model_factory: Some(ConfiguredPluginFactory::from_config(
                model::GenaiPlugin::default(),
            )),
            storage: Arc::new(storage::LocalStoragePlugin::new(database, artifact_root)),
            human_interaction: true,
            agent_tools: true,
            skills: Some(Arc::new(skills::SkillsPlugin::default())),
            agui: Some(Arc::new(agui::AgUiPlugin::default())),
            mcp: Vec::new(),
            telemetry: Vec::new(),
            read_only_filesystem: None,
            write_filesystem: None,
            system_commands: None,
            #[cfg(feature = "web")]
            web: None,
            #[cfg(feature = "media")]
            media: None,
            #[cfg(feature = "discovery")]
            discovery: None,
        }
    }

    /// Replace the default Genai plugin with configured Genai or a custom model plugin.
    pub fn model(mut self, plugin: impl Plugin + 'static) -> Self {
        self.model = Some(Arc::new(plugin));
        self.model_factory = None;
        self
    }

    pub fn model_factory(mut self, factory: ConfiguredPluginFactory) -> Self {
        self.model = None;
        self.model_factory = Some(factory);
        self
    }
    /// Supply model plugins separately, or use a loop that needs no model.
    /// FrameworkBuilder's independent `default_models` setting still applies.
    pub fn without_model(mut self) -> Self {
        self.model = None;
        self.model_factory = None;
        self
    }

    /// Replace the pack's local storage configuration.
    pub fn storage(mut self, plugin: storage::LocalStoragePlugin) -> Self {
        self.storage = Arc::new(plugin);
        self
    }

    pub fn human_interaction(mut self, enabled: bool) -> Self {
        self.human_interaction = enabled;
        self
    }

    /// Enable or omit the official child-agent and independent-thread tools.
    pub fn agent_tools(mut self, enabled: bool) -> Self {
        self.agent_tools = enabled;
        self
    }

    /// Replace the default filesystem skills provider or configure a custom one.
    pub fn skills(mut self, plugin: skills::SkillsPlugin) -> Self {
        self.skills = Some(Arc::new(plugin));
        self
    }

    /// Omit Skills context and tools for an Agent that does not use them.
    pub fn without_skills(mut self) -> Self {
        self.skills = None;
        self
    }

    /// Replace the default AG-UI configuration.
    pub fn agui(mut self, plugin: agui::AgUiPlugin) -> Self {
        self.agui = Some(Arc::new(plugin));
        self
    }

    /// Explicitly omit AG-UI output for an Agent that does not need it.
    pub fn without_agui(mut self) -> Self {
        self.agui = None;
        self
    }

    /// Add a configured MCP server, replacing the default empty tool source.
    /// Each configured plugin must have a distinct ID.
    pub fn mcp(mut self, plugin: mcp::McpToolsPlugin) -> Self {
        self.mcp.push(Arc::new(plugin));
        self
    }

    /// Supply explicit telemetry providers instead of the default global providers.
    /// Telemetry is installed before the pack's other plugins in middleware order.
    /// SDK provider ownership remains with the host, as defined by TelemetryPlugin.
    pub fn telemetry(mut self, plugin: telemetry::TelemetryPlugin) -> Self {
        self.telemetry.push(Arc::new(plugin));
        self
    }

    /// Explicitly enable directory-confined reading and search.
    pub fn read_only_filesystem(mut self, config: tools::FileSystemToolsConfig) -> Self {
        self.read_only_filesystem = Some(config);
        self
    }

    /// Explicitly enable directory-confined text writes and patches.
    pub fn write_filesystem(mut self, config: tools::FileSystemToolsConfig) -> Self {
        self.write_filesystem = Some(config);
        self
    }

    /// Explicitly enable command execution and task read/stop tools.
    pub fn system_commands(mut self, config: tools::SystemCommandConfig) -> Self {
        self.system_commands = Some(config);
        self
    }
    /// Explicitly select network tools and their host-owned backend/policy.
    #[cfg(feature = "web")]
    pub fn web(mut self, plugin: web::WebToolsPlugin) -> Self {
        self.web = Some(Arc::new(plugin));
        self
    }

    /// Explicitly select local image and PDF readers for a confined directory.
    #[cfg(feature = "media")]
    pub fn media(mut self, config: media::MediaToolsConfig) -> Self {
        self.media = Some(config);
        self
    }

    /// Defer explicitly supplied providers. Do not also install them separately.
    #[cfg(feature = "discovery")]
    pub fn tool_discovery(mut self, plugin: discovery::ToolDiscoveryPlugin) -> Self {
        self.discovery = Some(Arc::new(plugin));
        self
    }
}

impl CapabilityPack for EliteForgeOfficialCapabilityPack {
    fn id(&self) -> &str {
        "official"
    }
    fn configuration(&self) -> &CapabilityConfig {
        &()
    }
    fn included_packs(&self) -> Vec<Arc<dyn CapabilityPack>> {
        let mut packs: Vec<Arc<dyn CapabilityPack>> = Vec::new();
        let mut local = tools::LocalToolsConfig::default();
        let mut has_local = false;
        if let Some(config) = &self.read_only_filesystem {
            local = local.read_only_filesystem(config.clone());
            has_local = true;
        }
        if let Some(config) = &self.write_filesystem {
            local = local.write_filesystem(config.clone());
            has_local = true;
        }
        if let Some(config) = &self.system_commands {
            local = local.system_commands(config.clone());
            has_local = true;
        }
        if has_local {
            packs.push(Arc::new(tools::local_tools_pack(local)));
        }
        if self.human_interaction {
            packs.push(Arc::new(tools::human_interaction_pack()));
        }
        if self.agent_tools {
            packs.push(Arc::new(tools::agent_collaboration_pack()));
        }
        packs.push(Arc::new(tools::mcp_tools_pack(
            self.mcp.iter().map(|plugin| (**plugin).clone()),
        )));
        #[cfg(feature = "web")]
        if let Some(plugin) = &self.web {
            packs.push(Arc::new(tools::web_tools_pack((**plugin).clone())));
        }
        #[cfg(feature = "media")]
        if let Some(config) = &self.media {
            packs.push(Arc::new(tools::media_tools_pack(config.clone())));
        }
        #[cfg(feature = "discovery")]
        if let Some(plugin) = &self.discovery {
            packs.push(Arc::new(tools::tool_discovery_pack((**plugin).clone())));
        }
        packs
    }
    fn plugins(&self) -> Vec<Arc<dyn Plugin>> {
        let telemetry = if self.telemetry.is_empty() {
            vec![Arc::new(telemetry::TelemetryPlugin::default())]
        } else {
            self.telemetry.clone()
        };
        let mut plugins: Vec<Arc<dyn Plugin>> = telemetry
            .into_iter()
            .map(|plugin| ConfiguredPluginFactory::from_config((*plugin).clone()).plugin())
            .collect();
        plugins.push(ConfiguredPluginFactory::from_config((*self.storage).clone()).plugin());
        if let Some(factory) = &self.model_factory {
            plugins.push(factory.plugin());
        }
        if let Some(model) = &self.model {
            plugins.push(model.clone());
        }
        if let Some(skills) = &self.skills {
            plugins.push(ConfiguredPluginFactory::from_config((**skills).clone()).plugin());
        }
        if let Some(agui) = &self.agui {
            plugins.push(ConfiguredPluginFactory::from_config((**agui).clone()).plugin());
        }
        plugins
    }
}
