//! Official plugin configuration in one AgentBuilder::capability_pack call.
//!
//! The pack includes every official plugin: Genai, SQLite, human interaction,
//! agent collaboration, Skills, AG-UI, MCP and telemetry. Resource acquisition happens during Agent build;
//! all runtime operations and plugin shutdown remain owned by the Agent.

use eliteforge_agent_core::{CapabilityConfig, CapabilityPack, ConfiguredPluginFactory, Plugin};
use std::{path::PathBuf, sync::Arc};

pub use eliteforge_agent_official_plugin_agui as agui;
pub use eliteforge_agent_official_plugin_model_genai as model;
pub use eliteforge_agent_official_plugin_otel as telemetry;
pub use eliteforge_agent_official_plugin_skills as skills;
pub use eliteforge_agent_official_plugin_storage_sqlite as storage;
pub use eliteforge_agent_official_plugin_tools_agents as agents;
pub use eliteforge_agent_official_plugin_tools_human as human;
pub use eliteforge_agent_official_plugin_tools_mcp as mcp;

/// All official plugins with an explicit database path and configurable defaults.
///
/// Configure replacements on the pack itself. Adding a plugin with an existing
/// ID through AgentBuilder::plugin is an error, not an override.
pub struct EliteForgeOfficialCapabilityPack {
    model: Option<Arc<dyn Plugin>>,
    storage: Arc<storage::SqliteStoragePlugin>,
    model_factory: Option<ConfiguredPluginFactory>,
    human_interaction: bool,
    agent_tools: bool,
    skills: Option<Arc<skills::SkillsPlugin>>,
    agui: Option<Arc<agui::AgUiPlugin>>,
    mcp: Vec<Arc<mcp::McpToolsPlugin>>,
    telemetry: Vec<Arc<telemetry::TelemetryPlugin>>,
}

impl EliteForgeOfficialCapabilityPack {
    /// Register every official plugin. MCP starts with no servers; telemetry
    /// reads the host's global providers during installation.
    pub fn new(database: impl Into<PathBuf>) -> Self {
        Self {
            model: None,
            model_factory: Some(ConfiguredPluginFactory::from_config(
                model::GenaiPlugin::default(),
            )),
            storage: Arc::new(storage::SqliteStoragePlugin::new(database)),
            human_interaction: true,
            agent_tools: true,
            skills: Some(Arc::new(skills::SkillsPlugin::default())),
            agui: Some(Arc::new(agui::AgUiPlugin::default())),
            mcp: Vec::new(),
            telemetry: Vec::new(),
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
    /// AgentBuilder's independent `default_models` setting still applies.
    pub fn without_model(mut self) -> Self {
        self.model = None;
        self.model_factory = None;
        self
    }

    /// Replace the pack's SQLite configuration, including its database path.
    pub fn storage(mut self, plugin: storage::SqliteStoragePlugin) -> Self {
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
}

impl CapabilityPack for EliteForgeOfficialCapabilityPack {
    fn id(&self) -> &str {
        "official"
    }
    fn configuration(&self) -> &CapabilityConfig {
        &()
    }
    fn rebuildable(&self) -> bool {
        self.model.is_none()
    }
    fn retained_plugins(&self) -> Vec<Arc<dyn Plugin>> {
        self.model.iter().cloned().collect()
    }
    fn plugins(&self) -> Vec<Arc<dyn Plugin>> {
        let telemetry = if self.telemetry.is_empty() {
            vec![Arc::new(telemetry::TelemetryPlugin::default())]
        } else {
            self.telemetry.clone()
        };
        let mcp = if self.mcp.is_empty() {
            vec![Arc::new(mcp::McpToolsPlugin::default())]
        } else {
            self.mcp.clone()
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
        if self.human_interaction {
            plugins.push(Arc::new(human::HumanInteractionPlugin));
        }
        if self.agent_tools {
            plugins.push(Arc::new(agents::AgentToolsPlugin));
        }
        if let Some(skills) = &self.skills {
            plugins.push(ConfiguredPluginFactory::from_config((**skills).clone()).plugin());
        }
        if let Some(agui) = &self.agui {
            plugins.push(ConfiguredPluginFactory::from_config((**agui).clone()).plugin());
        }
        plugins.extend(
            mcp.into_iter()
                .map(|plugin| ConfiguredPluginFactory::from_config((*plugin).clone()).plugin()),
        );
        plugins
    }
}
