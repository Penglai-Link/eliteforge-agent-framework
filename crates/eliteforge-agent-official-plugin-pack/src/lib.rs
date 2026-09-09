//! Official plugin configuration in one AgentBuilder::plugin_pack call.
//!
//! The pack includes every official plugin: Genai, SQLite, human interaction,
//! AG-UI, MCP and telemetry. Resource acquisition happens during Agent build;
//! all runtime operations and plugin shutdown remain owned by the Agent.

use eliteforge_agent_core::{Plugin, PluginPack};
use std::{path::PathBuf, sync::Arc};

pub use eliteforge_agent_official_plugin_agui as agui;
pub use eliteforge_agent_official_plugin_model_genai as model;
pub use eliteforge_agent_official_plugin_otel as telemetry;
pub use eliteforge_agent_official_plugin_storage_sqlite as storage;
pub use eliteforge_agent_official_plugin_tools_human as human;
pub use eliteforge_agent_official_plugin_tools_mcp as mcp;

/// All official plugins with an explicit database path and configurable defaults.
///
/// Configure replacements on the pack itself. Adding a plugin with an existing
/// ID through AgentBuilder::plugin is an error, not an override.
pub struct EliteForgeOfficialPluginPack {
    model: Option<Arc<dyn Plugin>>,
    storage: storage::SqliteStoragePlugin,
    human_interaction: bool,
    agui: Option<agui::AgUiPlugin>,
    mcp: Vec<mcp::McpToolsPlugin>,
    telemetry: Vec<telemetry::TelemetryPlugin>,
}

impl EliteForgeOfficialPluginPack {
    /// Register every official plugin. MCP starts with no servers; telemetry
    /// reads the host's global providers during installation.
    pub fn new(database: impl Into<PathBuf>) -> Self {
        Self {
            model: Some(Arc::new(model::GenaiPlugin::default())),
            storage: storage::SqliteStoragePlugin::new(database),
            human_interaction: true,
            agui: Some(agui::AgUiPlugin::default()),
            mcp: Vec::new(),
            telemetry: Vec::new(),
        }
    }

    /// Replace the default Genai plugin with configured Genai or a custom model plugin.
    pub fn model(mut self, plugin: impl Plugin + 'static) -> Self {
        self.model = Some(Arc::new(plugin));
        self
    }

    /// Supply model plugins separately, or use a loop that needs no model.
    /// AgentBuilder's independent `default_models` setting still applies.
    pub fn without_model(mut self) -> Self {
        self.model = None;
        self
    }

    /// Replace the pack's SQLite configuration, including its database path.
    pub fn storage(mut self, plugin: storage::SqliteStoragePlugin) -> Self {
        self.storage = plugin;
        self
    }

    pub fn human_interaction(mut self, enabled: bool) -> Self {
        self.human_interaction = enabled;
        self
    }

    /// Replace the default AG-UI configuration.
    pub fn agui(mut self, plugin: agui::AgUiPlugin) -> Self {
        self.agui = Some(plugin);
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
        self.mcp.push(plugin);
        self
    }

    /// Supply explicit telemetry providers instead of the default global providers.
    /// Telemetry is installed before the pack's other plugins in middleware order.
    /// SDK provider ownership remains with the host, as defined by TelemetryPlugin.
    pub fn telemetry(mut self, plugin: telemetry::TelemetryPlugin) -> Self {
        self.telemetry.push(plugin);
        self
    }
}

impl PluginPack for EliteForgeOfficialPluginPack {
    fn into_plugins(self) -> Vec<Arc<dyn Plugin>> {
        let telemetry = if self.telemetry.is_empty() {
            vec![telemetry::TelemetryPlugin::default()]
        } else {
            self.telemetry
        };
        let mcp = if self.mcp.is_empty() {
            vec![mcp::McpToolsPlugin::default()]
        } else {
            self.mcp
        };
        let mut plugins: Vec<Arc<dyn Plugin>> = telemetry
            .into_iter()
            .map(|plugin| Arc::new(plugin) as Arc<dyn Plugin>)
            .collect();
        plugins.push(Arc::new(self.storage));
        if let Some(model) = self.model {
            plugins.push(model);
        }
        if self.human_interaction {
            plugins.push(Arc::new(human::HumanInteractionPlugin));
        }
        if let Some(agui) = self.agui {
            plugins.push(Arc::new(agui));
        }
        plugins.extend(
            mcp.into_iter()
                .map(|plugin| Arc::new(plugin) as Arc<dyn Plugin>),
        );
        plugins
    }
}
