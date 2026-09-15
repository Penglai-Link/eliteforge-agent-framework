//! Optional filesystem and system-command tools exposed through capability packs.
//!
//! The application chooses every pack explicitly. File and search operations are
//! confined to the configured directory. Command execution is fully general after
//! its capability pack is explicitly selected, and can be narrowed with an executable
//! allowlist. These tools do not provide a process sandbox.

mod filesystem;
mod tools;

use async_trait::async_trait;
use eliteforge_agent_core::{
    AgentError, CapabilityPackBuilder, RunContext, ToolLease, ToolProvider, capability,
};

pub use filesystem::{FileSystemToolsConfig, SystemCommandConfig};

/// Directory-confined file and search tools.
pub const READ_ONLY_FILESYSTEM_PACK: &str = "framework.filesystem.readonly";
/// Directory-confined text write and patch tools.
pub const WRITE_FILESYSTEM_PACK: &str = "framework.filesystem.write";
/// System-command tool. This pack does not create a process sandbox.
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

/// Select the system-command tool for one Agent.
pub fn system_command_pack(
    config: SystemCommandConfig,
) -> CapabilityPackBuilder<SystemCommandConfig> {
    CapabilityPackBuilder::new(SYSTEM_COMMAND_PACK, config)
}

struct ReadOnlyFileSystemTools {
    filesystem: filesystem::FileSystemRoot,
}

impl ReadOnlyFileSystemTools {
    fn from_config(config: &FileSystemToolsConfig) -> Self {
        Self {
            filesystem: filesystem::FileSystemRoot::new(config.clone()),
        }
    }
}

#[capability(
    pack = "framework.filesystem.readonly",
    id = "framework.filesystem.readonly.tools",
    instance = "framework.filesystem.readonly.tools",
    config = FileSystemToolsConfig,
    factory = ReadOnlyFileSystemTools::from_config
)]
#[async_trait]
impl ToolProvider for ReadOnlyFileSystemTools {
    async fn lease(&self, _: &RunContext) -> Result<ToolLease, AgentError> {
        Ok(ToolLease::new(tools::read_only_bindings(
            self.filesystem.clone(),
        )))
    }
}

struct WriteFileSystemTools {
    filesystem: filesystem::FileSystemRoot,
}

impl WriteFileSystemTools {
    fn from_config(config: &FileSystemToolsConfig) -> Self {
        Self {
            filesystem: filesystem::FileSystemRoot::new(config.clone()),
        }
    }
}

#[capability(
    pack = "framework.filesystem.write",
    id = "framework.filesystem.write.tools",
    instance = "framework.filesystem.write.tools",
    config = FileSystemToolsConfig,
    factory = WriteFileSystemTools::from_config
)]
#[async_trait]
impl ToolProvider for WriteFileSystemTools {
    async fn lease(&self, _: &RunContext) -> Result<ToolLease, AgentError> {
        Ok(ToolLease::new(tools::write_bindings(
            self.filesystem.clone(),
        )))
    }
}

struct SystemCommandTools {
    config: SystemCommandConfig,
}

impl SystemCommandTools {
    fn from_config(config: &SystemCommandConfig) -> Self {
        Self {
            config: config.clone(),
        }
    }
}

#[capability(
    pack = "framework.system-command",
    id = "framework.system-command.tools",
    instance = "framework.system-command.tools",
    config = SystemCommandConfig,
    factory = SystemCommandTools::from_config
)]
#[async_trait]
impl ToolProvider for SystemCommandTools {
    async fn lease(&self, _: &RunContext) -> Result<ToolLease, AgentError> {
        Ok(ToolLease::new(tools::system_command_bindings(
            self.config.clone(),
        )))
    }
}
