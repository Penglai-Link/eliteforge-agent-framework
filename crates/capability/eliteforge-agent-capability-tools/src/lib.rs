//! Explicit capability-pack selections for the official in-process local tools.
//! Tool implementation and resource cleanup belong to the local-tool plugins.

use eliteforge_agent_core::{CapabilityPackBuilder, capability_factory};
use eliteforge_agent_official_plugin_tools_local::{
    ReadOnlyFileSystemPlugin, SystemCommandPlugin, WriteFileSystemPlugin,
};

pub use eliteforge_agent_official_plugin_tools_local::{
    FileSystemToolsConfig, SystemCommandConfig,
};

pub const READ_ONLY_FILESYSTEM_PACK: &str = "framework.filesystem.readonly";
pub const WRITE_FILESYSTEM_PACK: &str = "framework.filesystem.write";
pub const SYSTEM_COMMAND_PACK: &str = "framework.system-command";

/// Select the read-only filesystem tools for one Agent.
pub fn read_only_filesystem_pack(
    config: FileSystemToolsConfig,
) -> CapabilityPackBuilder<FileSystemToolsConfig> {
    CapabilityPackBuilder::new(READ_ONLY_FILESYSTEM_PACK, config)
}

/// Select the mutating filesystem tools for one Agent.
pub fn write_filesystem_pack(
    config: FileSystemToolsConfig,
) -> CapabilityPackBuilder<FileSystemToolsConfig> {
    CapabilityPackBuilder::new(WRITE_FILESYSTEM_PACK, config)
}

/// Select literal command execution and its read/stop task tools for one Agent.
pub fn system_command_pack(
    config: SystemCommandConfig,
) -> CapabilityPackBuilder<SystemCommandConfig> {
    CapabilityPackBuilder::new(SYSTEM_COMMAND_PACK, config)
}

#[capability_factory(
    pack = "framework.filesystem.readonly",
    id = "framework.filesystem.readonly.tools",
    config = FileSystemToolsConfig
)]
fn read_only_tools(config: &FileSystemToolsConfig) -> ReadOnlyFileSystemPlugin {
    ReadOnlyFileSystemPlugin::new(config.clone())
}

#[capability_factory(
    pack = "framework.filesystem.write",
    id = "framework.filesystem.write.tools",
    config = FileSystemToolsConfig
)]
fn write_tools(config: &FileSystemToolsConfig) -> WriteFileSystemPlugin {
    WriteFileSystemPlugin::new(config.clone())
}

#[capability_factory(
    pack = "framework.system-command",
    id = "framework.system-command.tools",
    config = SystemCommandConfig
)]
fn command_tools(config: &SystemCommandConfig) -> SystemCommandPlugin {
    SystemCommandPlugin::new(config.clone())
}

pub use eliteforge_agent_official_plugin_tools_local::{
    PtyCommand,
    pty::{PtyEvent, PtyService, PtySnapshot, PtyStart},
};
pub const PTY_PACK: &str = "framework.pty";
/// Share one framework-owned terminal service between Agent and desktop adapters.
/// Hosts may construct the service before a lazy Agent starts; explicit close and
/// last-owner drop also terminate sessions when the Agent was never initialized.
pub fn pty_pack(service: PtyService) -> CapabilityPackBuilder<PtyService> {
    CapabilityPackBuilder::new(PTY_PACK, service)
}
#[capability_factory(pack = "framework.pty", id = "framework.pty.service", config = PtyService)]
fn pty_service(
    service: &PtyService,
) -> eliteforge_agent_official_plugin_tools_local::pty::PtyPlugin {
    eliteforge_agent_official_plugin_tools_local::pty::PtyPlugin(service.clone())
}

/// Scoped local tools and PTY are assembled together with one shared lifecycle.
pub fn scoped_local_tools_pack(
    provider: std::sync::Arc<
        dyn eliteforge_agent_official_plugin_tools_local::LocalToolScopeProvider,
    >,
    service: PtyService,
) -> CapabilityPackBuilder<()> {
    CapabilityPackBuilder::new("framework.local.scoped", ())
        .include(pty_pack(service.clone()))
        .plugin(
            eliteforge_agent_official_plugin_tools_local::ScopedLocalToolsPlugin::new(provider)
                .with_pty(service),
        )
}
