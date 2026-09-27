//! In-process local tool implementations, with command tasks owned by the Agent.
//! Filesystem paths are confined to the configured root. Commands use literal
//! arguments and optional executable allowlists; this is not a process sandbox.

/// Tool extension implementation description owned by this plugin package.
pub const TOOL_IMPLEMENTATION_DESCRIPTION: &str =
    "Provides workspace-scoped filesystem, command and shared PTY capabilities.";

pub mod capture;
mod edits;
mod filesystem;
mod patch_envelope;
pub mod process_tree;
pub mod pty;
mod pty_process;
pub mod pty_tools;
mod search;
mod tasks;
pub use pty_process::PtyCommand;
pub mod text_patch;
mod tools;

use async_trait::async_trait;
use eliteforge_agent_core::{
    AgentError, Plugin, PluginContext, PluginError, RunContext, ToolLease, ToolProvider,
    ToolProviderPlugin,
};

pub use filesystem::{FileSystemToolsConfig, SystemCommandConfig};

/// Host authority resolved once per run lease, never supplied by model arguments.
/// Implementations must validate the run's tenant/session permissions before
/// returning a directory grant. Returning an error grants no filesystem tools.
#[async_trait]
pub trait FileSystemScopeProvider: Send + Sync {
    async fn resolve(&self, run: &RunContext) -> Result<FileSystemToolsConfig, AgentError>;
}

#[derive(Clone)]
enum FileSystemScope {
    Fixed(filesystem::FileSystemRoot),
    Host(std::sync::Arc<dyn FileSystemScopeProvider>),
}

impl FileSystemScope {
    async fn lease(&self, run: &RunContext) -> Result<filesystem::FileSystemRoot, AgentError> {
        match self {
            Self::Fixed(root) => Ok(root.clone()),
            Self::Host(provider) => Ok(filesystem::FileSystemRoot::new(
                provider.resolve(run).await?,
            )),
        }
    }
    fn fresh(&self) -> Self {
        match self {
            Self::Fixed(root) => Self::Fixed(root.fresh()),
            Self::Host(provider) => Self::Host(provider.clone()),
        }
    }
}

/// Inert catalog for host policy/configuration UIs. Uses the same descriptors
/// as execution, without opening files, starting commands or installing a plugin.
/// The catalog grants no authority; hosts must still select the corresponding packs.
pub fn local_tool_catalog() -> Vec<eliteforge_agent_core::ToolDescriptor> {
    let root = filesystem::FileSystemRoot::new(FileSystemToolsConfig::new("."));
    tools::read_only_bindings(root.clone())
        .into_iter()
        .chain(tools::write_bindings(root))
        .chain(tools::system_command_bindings(
            SystemCommandConfig::new(),
            tasks::CommandTaskManager::default(),
        ))
        .map(|binding| binding.descriptor)
        .chain(pty_tools::terminal_tool_catalog())
        .collect()
}

/// Shared path confinement for optional local readers. Refuses absolute paths,
/// traversal and symlinks, and bounds the bytes read from a regular file.
#[derive(Clone)]
pub struct ConfinedFileReader(filesystem::FileSystemRoot);

pub struct ConfinedFile {
    pub path: String,
    pub bytes: Vec<u8>,
}

impl ConfinedFileReader {
    pub fn new(root: impl Into<std::path::PathBuf>) -> Self {
        Self(filesystem::FileSystemRoot::new(FileSystemToolsConfig::new(
            root,
        )))
    }

    pub fn read(
        &self,
        path: &str,
        max_bytes: usize,
    ) -> Result<ConfinedFile, eliteforge_agent_core::ToolError> {
        let resolved = self.0.resolve(path)?;
        let bytes = edits::read(&resolved.path, max_bytes)?.ok_or_else(|| {
            eliteforge_agent_core::ToolError::Failed("file does not exist".into())
        })?;
        Ok(ConfinedFile {
            path: self.0.display(&resolved.root, &resolved.path),
            bytes,
        })
    }
}

/// Directory-confined file reading, listing and search.
#[derive(Clone)]
pub struct ReadOnlyFileSystemPlugin {
    filesystem: FileSystemScope,
}

impl ReadOnlyFileSystemPlugin {
    pub fn new(config: FileSystemToolsConfig) -> Self {
        Self {
            filesystem: FileSystemScope::Fixed(filesystem::FileSystemRoot::new(config)),
        }
    }

    pub fn scoped(provider: std::sync::Arc<dyn FileSystemScopeProvider>) -> Self {
        Self {
            filesystem: FileSystemScope::Host(provider),
        }
    }
}

#[async_trait]
impl ToolProvider for ReadOnlyFileSystemPlugin {
    async fn lease(&self, run: &RunContext) -> Result<ToolLease, AgentError> {
        Ok(ToolLease::new(tools::read_only_bindings(
            self.filesystem.lease(run).await?,
        )))
    }
}

#[async_trait]
impl Plugin for ReadOnlyFileSystemPlugin {
    fn id(&self) -> &str {
        "framework.filesystem.readonly.tools"
    }

    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        ToolProviderPlugin::new(
            self.id(),
            Self {
                filesystem: self.filesystem.fresh(),
            },
        )
        .install(context)
        .await
    }
}

/// Directory-confined text writes and patches.
#[derive(Clone)]
pub struct WriteFileSystemPlugin {
    filesystem: FileSystemScope,
}

impl WriteFileSystemPlugin {
    pub fn new(config: FileSystemToolsConfig) -> Self {
        Self {
            filesystem: FileSystemScope::Fixed(filesystem::FileSystemRoot::new(config)),
        }
    }

    pub fn scoped(provider: std::sync::Arc<dyn FileSystemScopeProvider>) -> Self {
        Self {
            filesystem: FileSystemScope::Host(provider),
        }
    }
}

#[async_trait]
impl ToolProvider for WriteFileSystemPlugin {
    async fn lease(&self, run: &RunContext) -> Result<ToolLease, AgentError> {
        Ok(ToolLease::new(tools::write_bindings(
            self.filesystem.lease(run).await?,
        )))
    }
}

#[async_trait]
impl Plugin for WriteFileSystemPlugin {
    fn id(&self) -> &str {
        "framework.filesystem.write.tools"
    }

    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        ToolProviderPlugin::new(self.id(), self.clone())
            .install(context)
            .await
    }
}

/// Literal command execution, task output polling and task cancellation.
#[derive(Clone)]
pub struct SystemCommandPlugin {
    config: SystemCommandConfig,
}

impl SystemCommandPlugin {
    pub fn new(config: SystemCommandConfig) -> Self {
        Self { config }
    }
}

struct SystemCommandTools {
    config: SystemCommandConfig,
    tasks: tasks::CommandTaskManager,
}

#[async_trait]
impl ToolProvider for SystemCommandTools {
    async fn lease(&self, _: &RunContext) -> Result<ToolLease, AgentError> {
        Ok(ToolLease::new(tools::system_command_bindings(
            self.config.clone(),
            self.tasks.clone(),
        )))
    }
}

#[async_trait]
impl Plugin for SystemCommandPlugin {
    fn id(&self) -> &str {
        "framework.system-command.tools"
    }

    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let tasks = tasks::CommandTaskManager::default();
        let fallback_tasks = tasks.clone();
        context.on_cleanup(move || fallback_tasks.stop_all());
        let shutdown_tasks = tasks.clone();
        context.on_shutdown(move || async move { shutdown_tasks.shutdown().await });
        ToolProviderPlugin::new(
            self.id(),
            SystemCommandTools {
                config: self.config.clone(),
                tasks,
            },
        )
        .install(context)
        .await
    }
}

/// A host-issued local capability grant. Only selected IDs enter the run catalog;
/// an empty selection exposes nothing. Commands share one Agent-owned manager.
pub struct LocalToolGrant {
    /// Trusted terminal owner for product APIs sharing the same service.
    /// None uses the Framework session ID. Never copy this from model input.
    pub terminal_owner: Option<String>,
    pub filesystem: FileSystemToolsConfig,
    pub commands: SystemCommandConfig,
    pub tool_ids: std::collections::BTreeSet<String>,
    /// Optional compatibility names; IDs and executors remain official.
    pub call_names: std::collections::BTreeMap<String, String>,
    /// None exposes all authorized IDs; Some(empty) initially hides all.
    pub exposed_tool_ids: Option<std::collections::BTreeSet<String>>,
}

#[async_trait]
pub trait LocalToolScopeProvider: Send + Sync {
    async fn grant(&self, run: &RunContext) -> Result<LocalToolGrant, AgentError>;
}

/// Per-run directory and tool authority with Agent-lifetime command tasks.
/// This is the same executor/catalog used by the individual official plugins.
pub struct ScopedLocalToolsPlugin {
    pty: Option<pty::PtyService>,
    provider: std::sync::Arc<dyn LocalToolScopeProvider>,
}

impl ScopedLocalToolsPlugin {
    pub fn new(provider: std::sync::Arc<dyn LocalToolScopeProvider>) -> Self {
        Self {
            provider,
            pty: None,
        }
    }
    pub fn with_pty(mut self, service: pty::PtyService) -> Self {
        self.pty = Some(service);
        self
    }
}

struct ScopedLocalTools {
    pty: pty::PtyService,
    provider: std::sync::Arc<dyn LocalToolScopeProvider>,
    tasks: tasks::CommandTaskManager,
}

#[async_trait]
impl ToolProvider for ScopedLocalTools {
    async fn lease(&self, run: &RunContext) -> Result<ToolLease, AgentError> {
        let grant = self.provider.grant(run).await?;
        let root = filesystem::FileSystemRoot::new(grant.filesystem.clone());
        let mut bindings: Vec<_> = tools::read_only_bindings(root.clone())
            .into_iter()
            .chain(tools::write_bindings(root))
            .chain(tools::system_command_bindings(
                grant
                    .commands
                    .clone()
                    .with_directory_scope(grant.filesystem.clone()),
                self.tasks.clone(),
            ))
            .chain(pty_tools::bindings(
                self.pty.clone(),
                grant
                    .terminal_owner
                    .clone()
                    .unwrap_or_else(|| run.session_id.clone()),
                grant.commands.with_directory_scope(grant.filesystem),
            ))
            .collect();
        for id in &grant.tool_ids {
            if !bindings.iter().any(|binding| &binding.descriptor.id == id) {
                return Err(AgentError::ToolSetup(format!(
                    "unknown official local tool ID: {id}"
                )));
            }
        }
        if grant
            .call_names
            .keys()
            .any(|id| !grant.tool_ids.contains(id))
            || grant
                .exposed_tool_ids
                .as_ref()
                .is_some_and(|ids| !ids.is_subset(&grant.tool_ids))
        {
            return Err(AgentError::ToolSetup(
                "local tool aliases/exposure exceed the host grant".into(),
            ));
        }
        bindings.retain(|binding| grant.tool_ids.contains(&binding.descriptor.id));
        let aliases: Vec<_> = bindings
            .iter()
            .filter_map(|binding| {
                grant
                    .call_names
                    .get(&binding.descriptor.id)
                    .map(|alias| (binding.descriptor.call_name.clone(), alias.clone()))
            })
            .collect();
        for binding in &mut bindings {
            if let Some(name) = grant.call_names.get(&binding.descriptor.id) {
                if name.trim().is_empty() {
                    return Err(AgentError::ToolSetup("local tool alias is empty".into()));
                }
                binding.descriptor.call_name = name.clone();
            }
            for (old, new) in &aliases {
                binding.descriptor.description = binding.descriptor.description.replace(old, new);
            }
            if let Some(visible) = &grant.exposed_tool_ids {
                binding.exposed = visible.contains(&binding.descriptor.id);
            }
        }
        Ok(ToolLease::new(bindings))
    }
}

#[async_trait]
impl Plugin for ScopedLocalToolsPlugin {
    fn id(&self) -> &str {
        "framework.local.scoped.tools"
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let pty = self.pty.clone().unwrap_or_default();
        let cleanup_pty = pty.clone();
        context.on_cleanup(move || cleanup_pty.close());
        let shutdown_pty = pty.clone();
        context.on_shutdown(move || async move {
            tokio::task::spawn_blocking(move || shutdown_pty.close())
                .await
                .map_err(|error| error.to_string())
        });
        let tasks = tasks::CommandTaskManager::default();
        let cleanup = tasks.clone();
        context.on_cleanup(move || cleanup.stop_all());
        let shutdown = tasks.clone();
        context.on_shutdown(move || async move { shutdown.shutdown().await });
        ToolProviderPlugin::new(
            self.id(),
            ScopedLocalTools {
                pty,
                provider: self.provider.clone(),
                tasks,
            },
        )
        .install(context)
        .await
    }
}

/// Product-owned resource authority. The official text reader owns slicing,
/// UTF-8 validation and output bounds; a host resolves its opaque references.
/// Implementations must check workspace/session access and bound reads by max_bytes.
#[async_trait]
pub trait FileReferenceReader: Send + Sync {
    async fn read(
        &self,
        reference: serde_json::Value,
        max_bytes: usize,
        run: &RunContext,
    ) -> Result<ReferencedFile, eliteforge_agent_core::ToolError>;
}
pub struct ReferencedFile {
    pub name: String,
    pub bytes: Vec<u8>,
    pub reference: serde_json::Value,
}
#[derive(Clone)]
pub struct FileReferenceReaderService(pub std::sync::Arc<dyn FileReferenceReader>);

/// Optional host projection of command output while the initial invocation waits.
/// Results remain authoritative and cursor-readable; progress is presentation only.
pub trait CommandProgressSink: Send + Sync {
    fn chunk(&self, run: &RunContext, stream: &str, text: &str);
}
pub struct CommandProgressSinkService(pub std::sync::Arc<dyn CommandProgressSink>);
