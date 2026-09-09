use std::{
    collections::BTreeSet,
    fs,
    path::{Component, Path, PathBuf},
    time::Duration,
};

use eliteforge_agent_core::ToolError;

const DEFAULT_MAX_READ_BYTES: usize = 64 * 1024;
const DEFAULT_MAX_SEARCH_FILE_BYTES: usize = 256 * 1024;
const DEFAULT_MAX_RESULTS: usize = 200;
const DEFAULT_MAX_DEPTH: usize = 8;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 64 * 1024;
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// Bounds and directory authority for file and search tools.
#[derive(Debug, Clone)]
pub struct FileSystemToolsConfig {
    root: PathBuf,
    max_read_bytes: usize,
    max_search_file_bytes: usize,
    max_results: usize,
    max_depth: usize,
}

impl FileSystemToolsConfig {
    /// `root` must exist and be a directory when a tool is called.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            max_read_bytes: DEFAULT_MAX_READ_BYTES,
            max_search_file_bytes: DEFAULT_MAX_SEARCH_FILE_BYTES,
            max_results: DEFAULT_MAX_RESULTS,
            max_depth: DEFAULT_MAX_DEPTH,
        }
    }

    pub fn with_max_read_bytes(mut self, value: usize) -> Self {
        self.max_read_bytes = value.max(1);
        self
    }

    pub fn with_max_search_file_bytes(mut self, value: usize) -> Self {
        self.max_search_file_bytes = value.max(1);
        self
    }

    pub fn with_max_results(mut self, value: usize) -> Self {
        self.max_results = value.max(1);
        self
    }

    pub fn with_max_depth(mut self, value: usize) -> Self {
        self.max_depth = value;
        self
    }
}

/// Command authority for the system-command capability pack.
#[derive(Debug, Clone)]
pub struct SystemCommandConfig {
    command_authority: CommandAuthority,
    timeout: Duration,
    max_output_bytes: usize,
}

#[derive(Debug, Clone)]
enum CommandAuthority {
    All,
    Restricted(BTreeSet<String>),
}

impl SystemCommandConfig {
    /// Allow every executable. Selecting this capability pack remains explicit.
    pub fn new() -> Self {
        Self {
            command_authority: CommandAuthority::All,
            timeout: DEFAULT_COMMAND_TIMEOUT,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }

    /// Allow only exact executable names from `allowed_commands`.
    ///
    /// An empty collection deliberately denies every command; use [`Self::new`]
    /// when the selected capability pack should allow every executable.
    pub fn restricted<I, S>(allowed_commands: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            command_authority: CommandAuthority::Restricted(
                allowed_commands
                    .into_iter()
                    .map(Into::into)
                    .map(|command: String| command.trim().to_owned())
                    .filter(|command| !command.is_empty())
                    .collect(),
            ),
            timeout: DEFAULT_COMMAND_TIMEOUT,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout.max(Duration::from_millis(1));
        self
    }

    pub fn with_max_output_bytes(mut self, value: usize) -> Self {
        self.max_output_bytes = value.max(1);
        self
    }

    pub(crate) fn allows(&self, command: &str) -> bool {
        match &self.command_authority {
            CommandAuthority::All => true,
            CommandAuthority::Restricted(allowed_commands) => allowed_commands.contains(command),
        }
    }

    pub(crate) fn timeout(&self) -> Duration {
        self.timeout
    }

    pub(crate) fn max_output_bytes(&self) -> usize {
        self.max_output_bytes
    }
}

impl Default for SystemCommandConfig {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct FileSystemRoot {
    config: FileSystemToolsConfig,
}

pub(crate) struct ResolvedPath {
    pub(crate) root: PathBuf,
    pub(crate) path: PathBuf,
}

impl FileSystemRoot {
    pub(crate) fn new(config: FileSystemToolsConfig) -> Self {
        Self { config }
    }

    pub(crate) fn resolve(&self, raw: &str) -> Result<ResolvedPath, ToolError> {
        let root = fs::canonicalize(&self.config.root).map_err(|error| {
            ToolError::Failed(format!(
                "filesystem root `{}` is unavailable: {error}",
                self.config.root.display()
            ))
        })?;
        if !root.is_dir() {
            return Err(ToolError::Failed(format!(
                "filesystem root `{}` is not a directory",
                root.display()
            )));
        }

        let relative = relative_path(raw)?;
        let path = root.join(relative);
        ensure_no_symlinks(&root, &path)?;
        ensure_existing_ancestor_is_within(&root, &path)?;
        if path.exists() {
            let resolved = fs::canonicalize(&path).map_err(|error| {
                ToolError::Failed(format!("resolve `{}` failed: {error}", path.display()))
            })?;
            ensure_within(&root, &resolved)?;
        }
        Ok(ResolvedPath { root, path })
    }

    pub(crate) fn resolve_directory(&self, raw: Option<&str>) -> Result<ResolvedPath, ToolError> {
        let resolved = self.resolve(raw.unwrap_or("."))?;
        let metadata = fs::metadata(&resolved.path).map_err(|error| {
            ToolError::Failed(format!(
                "filesystem directory `{}` is unavailable: {error}",
                resolved.path.display()
            ))
        })?;
        if !metadata.is_dir() {
            return Err(ToolError::Failed(format!(
                "`{}` is not a directory",
                resolved.path.display()
            )));
        }
        Ok(resolved)
    }

    pub(crate) fn max_read_bytes(&self, requested: Option<usize>) -> usize {
        requested
            .unwrap_or(self.config.max_read_bytes)
            .clamp(1, self.config.max_read_bytes)
    }

    pub(crate) fn max_search_file_bytes(&self) -> usize {
        self.config.max_search_file_bytes
    }

    pub(crate) fn max_results(&self, requested: Option<usize>) -> usize {
        requested
            .unwrap_or(self.config.max_results)
            .clamp(1, self.config.max_results)
    }

    pub(crate) fn max_depth(&self, requested: Option<usize>) -> usize {
        requested
            .unwrap_or(self.config.max_depth)
            .min(self.config.max_depth)
    }

    pub(crate) fn display(&self, root: &Path, path: &Path) -> String {
        path.strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }
}

fn relative_path(raw: &str) -> Result<&Path, ToolError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(ToolError::Failed("path is required".into()));
    }
    let path = Path::new(raw);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(ToolError::Failed(
            "path must be relative to the configured filesystem root".into(),
        ));
    }
    Ok(path)
}

fn ensure_existing_ancestor_is_within(root: &Path, candidate: &Path) -> Result<(), ToolError> {
    let mut ancestor = candidate;
    while !ancestor.exists() {
        ancestor = ancestor.parent().ok_or_else(|| {
            ToolError::Failed("path has no ancestor inside the filesystem root".into())
        })?;
    }
    let resolved = fs::canonicalize(ancestor).map_err(|error| {
        ToolError::Failed(format!("resolve `{}` failed: {error}", ancestor.display()))
    })?;
    ensure_within(root, &resolved)
}

fn ensure_no_symlinks(root: &Path, candidate: &Path) -> Result<(), ToolError> {
    let relative = candidate
        .strip_prefix(root)
        .map_err(|_| ToolError::Failed("path escapes the configured filesystem root".into()))?;
    let mut current = root.to_owned();
    for component in relative.components() {
        current.push(component);
        if !current.exists() {
            break;
        }
        let metadata = fs::symlink_metadata(&current).map_err(|error| {
            ToolError::Failed(format!("inspect `{}` failed: {error}", current.display()))
        })?;
        if metadata.file_type().is_symlink() {
            return Err(ToolError::Failed(
                "symbolic links are not allowed in filesystem tool paths".into(),
            ));
        }
    }
    Ok(())
}

fn ensure_within(root: &Path, candidate: &Path) -> Result<(), ToolError> {
    if candidate.starts_with(root) {
        Ok(())
    } else {
        Err(ToolError::Failed(
            "path escapes the configured filesystem root".into(),
        ))
    }
}
