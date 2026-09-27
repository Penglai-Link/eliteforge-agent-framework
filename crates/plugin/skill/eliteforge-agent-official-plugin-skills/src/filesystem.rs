use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File},
    io::{BufRead, BufReader, Read},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
    time::SystemTime,
};

use async_trait::async_trait;
use eliteforge_agent_core::{
    AgentError, RunContext, SkillCatalog, SkillDetail, SkillDiagnostic, SkillFile, SkillProvider,
    SkillResource, SkillSummary,
};
use serde::Deserialize;
use tokio::sync::OnceCell;

const SKILL_MARKDOWN: &str = "SKILL.md";
const DEFAULT_MAX_SKILLS: usize = 256;
const DEFAULT_MAX_FRONTMATTER_BYTES: usize = 64 * 1024;
const DEFAULT_MAX_SKILL_MARKDOWN_BYTES: usize = 512 * 1024;
const DEFAULT_MAX_RESOURCE_BYTES: usize = 2 * 1024 * 1024;
const DEFAULT_MAX_RESOURCES_PER_SKILL: usize = 512;
const DEFAULT_MAX_RESOURCE_DEPTH: usize = 8;

/// Filesystem source and read limits for standard Agent Skills.
///
/// `Default` captures `~/.agents/skills` (or `%USERPROFILE%\\.agents\\skills`)
/// and `.agents/skills` below the current working directory when the config is
/// created. Missing directories are empty catalogs, not errors.
#[derive(Debug, Clone)]
pub struct FileSystemSkillsConfig {
    system_directory: Option<PathBuf>,
    local_directory: Option<PathBuf>,
    max_skills: usize,
    max_frontmatter_bytes: usize,
    max_skill_markdown_bytes: usize,
    max_resource_bytes: usize,
    max_resources_per_skill: usize,
    max_resource_depth: usize,
}

impl Default for FileSystemSkillsConfig {
    fn default() -> Self {
        Self {
            system_directory: default_system_directory(),
            local_directory: std::env::current_dir()
                .ok()
                .map(|directory| directory.join(".agents").join("skills")),
            max_skills: DEFAULT_MAX_SKILLS,
            max_frontmatter_bytes: DEFAULT_MAX_FRONTMATTER_BYTES,
            max_skill_markdown_bytes: DEFAULT_MAX_SKILL_MARKDOWN_BYTES,
            max_resource_bytes: DEFAULT_MAX_RESOURCE_BYTES,
            max_resources_per_skill: DEFAULT_MAX_RESOURCES_PER_SKILL,
            max_resource_depth: DEFAULT_MAX_RESOURCE_DEPTH,
        }
    }
}

impl FileSystemSkillsConfig {
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the system-level source directory.
    pub fn with_system_directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.system_directory = Some(directory.into());
        self
    }

    /// Do not scan a system-level source directory.
    pub fn without_system_directory(mut self) -> Self {
        self.system_directory = None;
        self
    }

    /// Override the current-directory source directory.
    pub fn with_local_directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.local_directory = Some(directory.into());
        self
    }

    /// Do not scan a local source directory.
    pub fn without_local_directory(mut self) -> Self {
        self.local_directory = None;
        self
    }

    pub fn with_max_skills(mut self, value: usize) -> Self {
        self.max_skills = value;
        self
    }

    pub fn with_max_frontmatter_bytes(mut self, value: usize) -> Self {
        self.max_frontmatter_bytes = value;
        self
    }

    pub fn with_max_skill_markdown_bytes(mut self, value: usize) -> Self {
        self.max_skill_markdown_bytes = value;
        self
    }

    pub fn with_max_resource_bytes(mut self, value: usize) -> Self {
        self.max_resource_bytes = value;
        self
    }

    pub fn with_max_resources_per_skill(mut self, value: usize) -> Self {
        self.max_resources_per_skill = value;
        self
    }

    pub fn with_max_resource_depth(mut self, value: usize) -> Self {
        self.max_resource_depth = value;
        self
    }

    fn validate(&self) -> Result<(), AgentError> {
        if self.max_skills == 0
            || self.max_frontmatter_bytes == 0
            || self.max_skill_markdown_bytes == 0
            || self.max_resource_bytes == 0
            || self.max_resources_per_skill == 0
        {
            return Err(AgentError::InvalidConfiguration(
                "skill discovery and read limits must be positive".into(),
            ));
        }
        Ok(())
    }
}

fn default_system_directory() -> Option<PathBuf> {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    home.map(PathBuf::from)
        .map(|directory| directory.join(".agents").join("skills"))
}

/// Filesystem implementation of [`SkillProvider`].
///
/// A provider stores one discovered snapshot per logical run ID. The snapshot
/// survives an in-process resume and prevents a later model step from combining
/// old metadata with changed skill content; a changed file is rejected until a
/// new logical run discovers it.
#[derive(Clone)]
pub struct FileSystemSkillProvider {
    config: FileSystemSkillsConfig,
    snapshots: FileSystemSnapshots,
}

type FileSystemSnapshotSlot = Arc<OnceCell<Arc<FileSystemSnapshot>>>;
type FileSystemSnapshots =
    Arc<Mutex<HashMap<eliteforge_agent_core::RunId, FileSystemSnapshotSlot>>>;

impl FileSystemSkillProvider {
    pub fn new(config: FileSystemSkillsConfig) -> Self {
        Self {
            config,
            snapshots: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Scan the configured directories without retaining a logical-run snapshot.
    /// This is useful for upper-application diagnostics and installation checks.
    pub async fn discover(&self) -> Result<SkillCatalog, AgentError> {
        Ok(self.discover_snapshot().await?.catalog.clone())
    }

    async fn snapshot(&self, context: &RunContext) -> Result<Arc<FileSystemSnapshot>, AgentError> {
        let run_id = context.run_id;
        let slot = self
            .snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(run_id)
            .or_insert_with(|| Arc::new(OnceCell::new()))
            .clone();
        let snapshot = slot
            .get_or_try_init(|| async {
                Ok::<_, AgentError>(Arc::new(self.discover_snapshot().await?))
            })
            .await?;
        Ok(snapshot.clone())
    }

    async fn discover_snapshot(&self) -> Result<FileSystemSnapshot, AgentError> {
        let config = self.config.clone();
        tokio::task::spawn_blocking(move || discover(config))
            .await
            .map_err(|error| AgentError::Run(format!("skill discovery task failed: {error}")))?
    }
}

#[async_trait]
impl SkillProvider for FileSystemSkillProvider {
    async fn catalog(&self, context: &RunContext) -> Result<SkillCatalog, AgentError> {
        Ok(self.snapshot(context).await?.catalog.clone())
    }

    async fn load(&self, context: &RunContext, name: &str) -> Result<SkillDetail, AgentError> {
        let snapshot = self.snapshot(context).await?;
        let name = name.to_owned();
        let limit = self.config.max_skill_markdown_bytes;
        tokio::task::spawn_blocking(move || load_skill(snapshot, &name, limit))
            .await
            .map_err(|error| AgentError::Run(format!("skill load task failed: {error}")))?
    }

    async fn read_resource(
        &self,
        context: &RunContext,
        name: &str,
        path: &str,
    ) -> Result<SkillResource, AgentError> {
        let snapshot = self.snapshot(context).await?;
        let name = name.to_owned();
        let path = path.to_owned();
        let limit = self.config.max_resource_bytes;
        tokio::task::spawn_blocking(move || read_resource(snapshot, &name, &path, limit))
            .await
            .map_err(|error| AgentError::Run(format!("skill resource task failed: {error}")))?
    }

    fn release(&self, context: &RunContext) {
        self.snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&context.run_id);
    }
}

struct FileSystemSnapshot {
    catalog: SkillCatalog,
    skills: BTreeMap<String, FileSystemSkill>,
}

struct FileSystemSkill {
    summary: SkillSummary,
    root: PathBuf,
    markdown: FileEntry,
    files: BTreeMap<String, FileEntry>,
    markdown_cache: Mutex<Option<String>>,
    resource_cache: Mutex<BTreeMap<String, String>>,
}

#[derive(Clone)]
struct FileEntry {
    path: PathBuf,
    fingerprint: FileFingerprint,
}

#[derive(Clone, PartialEq, Eq)]
struct FileFingerprint {
    length: u64,
    modified: Option<SystemTime>,
}

#[derive(Deserialize)]
struct SkillFrontmatter {
    name: String,
    description: String,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    compatibility: Option<String>,
    #[serde(default)]
    metadata: BTreeMap<String, String>,
    #[serde(rename = "allowed-tools", default)]
    allowed_tools: Option<String>,
}

fn discover(config: FileSystemSkillsConfig) -> Result<FileSystemSnapshot, AgentError> {
    config.validate()?;
    let mut diagnostics = Vec::new();
    let mut skills = BTreeMap::new();

    for (source, directory) in [
        ("system", config.system_directory.as_deref()),
        ("local", config.local_directory.as_deref()),
    ] {
        let scanned = scan_source(source, directory, &config)?;
        diagnostics.extend(scanned.diagnostics);
        for (name, skill) in scanned.skills {
            skills.insert(name, skill);
        }
    }

    if skills.len() > config.max_skills {
        return Err(AgentError::InvalidConfiguration(format!(
            "skill discovery found more than the configured maximum of {} skills",
            config.max_skills
        )));
    }

    let catalog = SkillCatalog {
        skills: skills.values().map(|skill| skill.summary.clone()).collect(),
        diagnostics,
    };
    Ok(FileSystemSnapshot { catalog, skills })
}

struct ScannedSource {
    skills: BTreeMap<String, FileSystemSkill>,
    diagnostics: Vec<SkillDiagnostic>,
}

fn scan_source(
    source: &str,
    directory: Option<&Path>,
    config: &FileSystemSkillsConfig,
) -> Result<ScannedSource, AgentError> {
    let Some(directory) = directory else {
        return Ok(ScannedSource {
            skills: BTreeMap::new(),
            diagnostics: Vec::new(),
        });
    };

    let metadata = match fs::metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ScannedSource {
                skills: BTreeMap::new(),
                diagnostics: Vec::new(),
            });
        }
        Err(error) => {
            return Err(AgentError::Run(format!(
                "skill directory `{}` is unavailable: {error}",
                directory.display()
            )));
        }
    };
    if !metadata.is_dir() {
        return Err(AgentError::InvalidConfiguration(format!(
            "skill directory `{}` is not a directory",
            directory.display()
        )));
    }
    let directory = fs::canonicalize(directory).map_err(|error| {
        AgentError::Run(format!(
            "resolve skill directory `{}` failed: {error}",
            directory.display()
        ))
    })?;

    let entries = fs::read_dir(&directory).map_err(|error| {
        AgentError::Run(format!(
            "read skill directory `{}` failed: {error}",
            directory.display()
        ))
    })?;
    let mut entries = entries.collect::<Result<Vec<_>, _>>().map_err(|error| {
        AgentError::Run(format!(
            "read entries in skill directory `{}` failed: {error}",
            directory.display()
        ))
    })?;
    entries.sort_by_key(|entry| entry.file_name());

    let mut skills = BTreeMap::new();
    let mut diagnostics = Vec::new();
    for entry in entries {
        let path = entry.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                diagnostics.push(diagnostic(
                    &path,
                    format!("could not inspect skill candidate: {error}"),
                ));
                continue;
            }
        };
        if metadata.file_type().is_symlink() {
            diagnostics.push(diagnostic(
                &path,
                "symbolic-link skill directories are ignored",
            ));
            continue;
        }
        if !metadata.is_dir() {
            continue;
        }
        let root = match fs::canonicalize(&path) {
            Ok(root) if root.starts_with(&directory) => root,
            Ok(_) => {
                diagnostics.push(diagnostic(
                    &path,
                    "skill directory escapes its configured source",
                ));
                continue;
            }
            Err(error) => {
                diagnostics.push(diagnostic(
                    &path,
                    format!("could not resolve skill directory: {error}"),
                ));
                continue;
            }
        };
        let markdown_path = root.join(SKILL_MARKDOWN);
        let markdown_metadata = match fs::symlink_metadata(&markdown_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                diagnostics.push(diagnostic(
                    &markdown_path,
                    format!("could not inspect SKILL.md: {error}"),
                ));
                continue;
            }
        };
        if markdown_metadata.file_type().is_symlink() || !markdown_metadata.is_file() {
            diagnostics.push(diagnostic(
                &markdown_path,
                "SKILL.md must be a regular file and cannot be a symbolic link",
            ));
            continue;
        }

        let frontmatter = match read_frontmatter(&markdown_path, config.max_frontmatter_bytes) {
            Ok(frontmatter) => frontmatter,
            Err(message) => {
                diagnostics.push(diagnostic(&markdown_path, message));
                continue;
            }
        };
        if let Err(message) = validate_frontmatter(&frontmatter, &root) {
            diagnostics.push(diagnostic(&markdown_path, message));
            continue;
        }

        let (files, resource_diagnostics) = match scan_resources(&root, config) {
            Ok(files) => files,
            Err(message) => {
                diagnostics.push(diagnostic(&root, message));
                continue;
            }
        };
        diagnostics.extend(resource_diagnostics);
        let markdown = FileEntry {
            path: markdown_path,
            fingerprint: fingerprint(&markdown_metadata),
        };
        let summary = SkillSummary {
            name: frontmatter.name.clone(),
            description: frontmatter.description,
            source: source.to_owned(),
            location: root.to_string_lossy().to_string(),
            license: frontmatter.license,
            compatibility: frontmatter.compatibility,
            metadata: frontmatter.metadata,
            allowed_tools: frontmatter.allowed_tools,
        };
        skills.insert(
            summary.name.clone(),
            FileSystemSkill {
                summary,
                root,
                markdown,
                files,
                markdown_cache: Mutex::new(None),
                resource_cache: Mutex::new(BTreeMap::new()),
            },
        );
    }
    Ok(ScannedSource {
        skills,
        diagnostics,
    })
}

fn read_frontmatter(path: &Path, max_bytes: usize) -> Result<SkillFrontmatter, String> {
    let file = File::open(path).map_err(|error| format!("SKILL.md could not be read: {error}"))?;
    let mut reader = BufReader::new(file.take((max_bytes.saturating_add(1)) as u64));
    let mut line = String::new();
    let first = reader
        .read_line(&mut line)
        .map_err(|_| "SKILL.md frontmatter is not valid UTF-8".to_string())?;
    if first == 0 || strip_line_end(&line).trim_start_matches('\u{feff}') != "---" {
        return Err("SKILL.md must start with YAML frontmatter".into());
    }
    let mut consumed = first;
    let mut yaml = String::new();
    loop {
        line.clear();
        let count = reader
            .read_line(&mut line)
            .map_err(|_| "SKILL.md frontmatter is not valid UTF-8".to_string())?;
        if count == 0 {
            return Err("SKILL.md frontmatter is missing its closing delimiter".into());
        }
        consumed = consumed.saturating_add(count);
        if consumed > max_bytes {
            return Err(format!(
                "SKILL.md frontmatter exceeds the configured {max_bytes}-byte limit"
            ));
        }
        if strip_line_end(&line) == "---" {
            break;
        }
        yaml.push_str(&line);
    }
    serde_norway::from_str(&yaml).map_err(|_| "SKILL.md frontmatter is invalid YAML".into())
}

fn strip_line_end(value: &str) -> &str {
    value.trim_end_matches(&['\r', '\n'][..])
}

fn validate_frontmatter(frontmatter: &SkillFrontmatter, root: &Path) -> Result<(), String> {
    if !valid_skill_name(&frontmatter.name) {
        return Err(
            "SKILL.md frontmatter name must use 1-64 lowercase Unicode letters, digits, or hyphens"
                .into(),
        );
    }
    if root.file_name().and_then(|name| name.to_str()) != Some(frontmatter.name.as_str()) {
        return Err("SKILL.md frontmatter name must match its directory name".into());
    }
    if frontmatter.description.trim().is_empty() || frontmatter.description.chars().count() > 1024 {
        return Err("SKILL.md frontmatter description must contain 1-1024 characters".into());
    }
    if frontmatter
        .compatibility
        .as_deref()
        .is_some_and(|value| value.is_empty() || value.chars().count() > 500)
    {
        return Err("SKILL.md compatibility must contain 1-500 characters when provided".into());
    }
    Ok(())
}

fn valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= 64
        && name == name.to_lowercase()
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
        && name
            .chars()
            .all(|character| character.is_alphanumeric() || character == '-')
}

fn scan_resources(
    root: &Path,
    config: &FileSystemSkillsConfig,
) -> Result<(BTreeMap<String, FileEntry>, Vec<SkillDiagnostic>), String> {
    let mut files = BTreeMap::new();
    let mut diagnostics = Vec::new();
    visit_resources(root, root, 0, config, &mut files, &mut diagnostics)?;
    Ok((files, diagnostics))
}

fn visit_resources(
    root: &Path,
    directory: &Path,
    depth: usize,
    config: &FileSystemSkillsConfig,
    files: &mut BTreeMap<String, FileEntry>,
    diagnostics: &mut Vec<SkillDiagnostic>,
) -> Result<(), String> {
    let entries = fs::read_dir(directory).map_err(|error| {
        format!(
            "could not read skill resources in `{}`: {error}",
            directory.display()
        )
    })?;
    let mut entries = entries.collect::<Result<Vec<_>, _>>().map_err(|error| {
        format!(
            "could not list skill resources in `{}`: {error}",
            directory.display()
        )
    })?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let relative = relative_resource_path(root, &path)?;
        if relative == SKILL_MARKDOWN {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("could not inspect skill resource `{relative}`: {error}"))?;
        if metadata.file_type().is_symlink() {
            diagnostics.push(diagnostic(
                &path,
                "symbolic-link skill resources are ignored",
            ));
            continue;
        }
        if metadata.is_dir() {
            if depth >= config.max_resource_depth {
                diagnostics.push(diagnostic(
                    &path,
                    "skill resources below the configured maximum depth are ignored",
                ));
                continue;
            }
            visit_resources(root, &path, depth + 1, config, files, diagnostics)?;
            continue;
        }
        if !metadata.is_file() {
            diagnostics.push(diagnostic(&path, "non-regular skill resource is ignored"));
            continue;
        }
        if files.len() >= config.max_resources_per_skill {
            return Err(format!(
                "skill has more than the configured {} resource files",
                config.max_resources_per_skill
            ));
        }
        files.insert(
            relative,
            FileEntry {
                path,
                fingerprint: fingerprint(&metadata),
            },
        );
    }
    Ok(())
}

fn relative_resource_path(root: &Path, path: &Path) -> Result<String, String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| "skill resource escapes its root".to_string())?;
    let mut parts = Vec::new();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err("skill resource path is not a normal relative path".into());
        };
        parts.push(
            part.to_str()
                .ok_or_else(|| "skill resource path is not valid UTF-8".to_string())?,
        );
    }
    if parts.is_empty() {
        return Err("skill resource path is empty".into());
    }
    Ok(parts.join("/"))
}

fn load_skill(
    snapshot: Arc<FileSystemSnapshot>,
    name: &str,
    max_bytes: usize,
) -> Result<SkillDetail, AgentError> {
    let skill = snapshot
        .skills
        .get(name)
        .ok_or_else(|| AgentError::Run(format!("unknown skill `{name}`")))?;
    if let Some(markdown) = skill
        .markdown_cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
    {
        return Ok(skill_detail(skill, markdown));
    }
    let markdown = read_stable_text(
        &skill.root,
        &skill.markdown,
        max_bytes,
        &format!("skill `{name}`"),
    )?;
    *skill
        .markdown_cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(markdown.clone());
    Ok(skill_detail(skill, markdown))
}

fn skill_detail(skill: &FileSystemSkill, markdown: String) -> SkillDetail {
    SkillDetail {
        skill: skill.summary.clone(),
        skill_markdown: markdown,
        files: skill
            .files
            .iter()
            .map(|(path, file)| SkillFile {
                path: path.clone(),
                size_bytes: file.fingerprint.length,
            })
            .collect(),
    }
}

fn read_resource(
    snapshot: Arc<FileSystemSnapshot>,
    name: &str,
    raw_path: &str,
    max_bytes: usize,
) -> Result<SkillResource, AgentError> {
    let path = normalize_requested_resource_path(raw_path)?;
    let skill = snapshot
        .skills
        .get(name)
        .ok_or_else(|| AgentError::Run(format!("unknown skill `{name}`")))?;
    let file = skill.files.get(&path).ok_or_else(|| {
        AgentError::Run(format!("resource `{path}` is not listed by skill `{name}`"))
    })?;
    if let Some(content) = skill
        .resource_cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&path)
        .cloned()
    {
        return Ok(SkillResource {
            skill_name: name.to_owned(),
            path,
            content,
        });
    }
    let content = read_stable_text(
        &skill.root,
        file,
        max_bytes,
        &format!("skill resource `{path}`"),
    )?;
    skill
        .resource_cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(path.clone(), content.clone());
    Ok(SkillResource {
        skill_name: name.to_owned(),
        path,
        content,
    })
}

fn normalize_requested_resource_path(raw_path: &str) -> Result<String, AgentError> {
    let raw_path = raw_path.trim();
    let path = Path::new(raw_path);
    if raw_path.is_empty() || path.is_absolute() {
        return Err(AgentError::Run(
            "skill resource path must be a normal relative path".into(),
        ));
    }
    let mut parts = Vec::new();
    for component in path.components() {
        let Component::Normal(part) = component else {
            return Err(AgentError::Run(
                "skill resource path must be a normal relative path".into(),
            ));
        };
        parts
            .push(part.to_str().ok_or_else(|| {
                AgentError::Run("skill resource path must be valid UTF-8".into())
            })?);
    }
    if parts.is_empty() {
        return Err(AgentError::Run(
            "skill resource path must be a normal relative path".into(),
        ));
    }
    let path = parts.join("/");
    if path == SKILL_MARKDOWN {
        return Err(AgentError::Run(
            "load SKILL.md with skill_load instead of skill_read_resource".into(),
        ));
    }
    Ok(path)
}

fn read_stable_text(
    root: &Path,
    entry: &FileEntry,
    max_bytes: usize,
    label: &str,
) -> Result<String, AgentError> {
    ensure_stable_regular_file(root, entry, label)?;
    let file = File::open(&entry.path)
        .map_err(|error| AgentError::Run(format!("read {label} failed: {error}")))?;
    let opened = file
        .metadata()
        .map_err(|error| AgentError::Run(format!("inspect {label} failed: {error}")))?;
    if fingerprint(&opened) != entry.fingerprint {
        return Err(changed_skill_error(label));
    }
    read_utf8_limited(file, max_bytes, label)
}

fn ensure_stable_regular_file(
    root: &Path,
    entry: &FileEntry,
    label: &str,
) -> Result<(), AgentError> {
    let relative = entry
        .path
        .strip_prefix(root)
        .map_err(|_| AgentError::Run(format!("{label} escapes its skill directory")))?;
    let mut current = root.to_owned();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(AgentError::Run(format!("{label} has an invalid path")));
        };
        current.push(component);
        let metadata = fs::symlink_metadata(&current)
            .map_err(|error| AgentError::Run(format!("inspect {label} failed: {error}")))?;
        if metadata.file_type().is_symlink() {
            return Err(AgentError::Run(format!(
                "{label} cannot be read through a symbolic link"
            )));
        }
    }
    let metadata = fs::symlink_metadata(&entry.path)
        .map_err(|error| AgentError::Run(format!("inspect {label} failed: {error}")))?;
    if !metadata.is_file() || fingerprint(&metadata) != entry.fingerprint {
        return Err(changed_skill_error(label));
    }
    let resolved = fs::canonicalize(&entry.path)
        .map_err(|error| AgentError::Run(format!("resolve {label} failed: {error}")))?;
    if !resolved.starts_with(root) {
        return Err(AgentError::Run(format!(
            "{label} escapes its skill directory"
        )));
    }
    Ok(())
}

fn read_utf8_limited(file: File, max_bytes: usize, label: &str) -> Result<String, AgentError> {
    let mut bytes = Vec::new();
    file.take((max_bytes.saturating_add(1)) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| AgentError::Run(format!("read {label} failed: {error}")))?;
    if bytes.len() > max_bytes {
        return Err(AgentError::Run(format!(
            "{label} exceeds the configured {max_bytes}-byte read limit"
        )));
    }
    String::from_utf8(bytes).map_err(|_| AgentError::Run(format!("{label} is not valid UTF-8")))
}

fn fingerprint(metadata: &fs::Metadata) -> FileFingerprint {
    FileFingerprint {
        length: metadata.len(),
        modified: metadata.modified().ok(),
    }
}

fn changed_skill_error(label: &str) -> AgentError {
    AgentError::Run(format!(
        "{label} changed after its metadata was discovered; start a new execution"
    ))
}

fn diagnostic(path: &Path, message: impl Into<String>) -> SkillDiagnostic {
    SkillDiagnostic {
        location: path.to_string_lossy().to_string(),
        message: message.into(),
    }
}
