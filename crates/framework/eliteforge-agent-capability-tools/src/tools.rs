use std::{
    collections::BTreeSet,
    fs,
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
};

use async_trait::async_trait;
use eliteforge_agent_core::{RunContext, Tool, ToolBinding, ToolDescriptor, ToolError, ToolOutput};
use regex::RegexBuilder;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use crate::filesystem::{FileSystemRoot, SystemCommandConfig};

pub(crate) fn read_only_bindings(filesystem: FileSystemRoot) -> Vec<ToolBinding> {
    vec![
        ToolBinding::new(
            read_text_descriptor(),
            ReadTextTool {
                filesystem: filesystem.clone(),
            },
        ),
        ToolBinding::new(
            list_descriptor(),
            ListTool {
                filesystem: filesystem.clone(),
            },
        ),
        ToolBinding::new(
            stat_descriptor(),
            StatTool {
                filesystem: filesystem.clone(),
            },
        ),
        ToolBinding::new(
            grep_descriptor(),
            GrepTool {
                filesystem: filesystem.clone(),
            },
        )
        .parallel_safe(),
        ToolBinding::new(
            glob_descriptor(),
            GlobTool {
                filesystem: filesystem.clone(),
            },
        )
        .parallel_safe(),
        ToolBinding::new(path_descriptor(), PathTool { filesystem }).parallel_safe(),
    ]
}

pub(crate) fn write_bindings(filesystem: FileSystemRoot) -> Vec<ToolBinding> {
    vec![
        ToolBinding::new(
            write_text_descriptor(),
            WriteTextTool {
                filesystem: filesystem.clone(),
            },
        ),
        ToolBinding::new(patch_descriptor(), PatchTool { filesystem }),
    ]
}

pub(crate) fn system_command_bindings(config: SystemCommandConfig) -> Vec<ToolBinding> {
    vec![ToolBinding::new(
        system_command_descriptor(),
        SystemCommandTool { config },
    )]
}

fn read_text_descriptor() -> ToolDescriptor {
    ToolDescriptor::new(
        "filesystem.read_text",
        "fs_read_text",
        "Read UTF-8 text from a file below the configured filesystem root.",
        json!({
            "type":"object", "additionalProperties":false, "required":["path"],
            "properties": {
                "path":{"type":"string", "minLength":1},
                "startLine":{"type":"integer", "minimum":1},
                "endLine":{"type":"integer", "minimum":1},
                "maxBytes":{"type":"integer", "minimum":1}
            }
        }),
    )
}

fn list_descriptor() -> ToolDescriptor {
    ToolDescriptor::new(
        "filesystem.list",
        "fs_list",
        "List files and directories below the configured filesystem root without following symlinks.",
        json!({
            "type":"object", "additionalProperties":false, "required":["path"],
            "properties": {
                "path":{"type":"string", "minLength":1},
                "recursive":{"type":"boolean"},
                "maxDepth":{"type":"integer", "minimum":0},
                "includeHidden":{"type":"boolean"},
                "filesOnly":{"type":"boolean"},
                "directoriesOnly":{"type":"boolean"},
                "maxResults":{"type":"integer", "minimum":1}
            }
        }),
    )
}

fn stat_descriptor() -> ToolDescriptor {
    ToolDescriptor::new(
        "filesystem.stat",
        "fs_stat",
        "Read metadata for a filesystem file or directory without following symlinks.",
        json!({
            "type":"object", "additionalProperties":false, "required":["path"],
            "properties": { "path":{"type":"string", "minLength":1} }
        }),
    )
}

fn grep_descriptor() -> ToolDescriptor {
    ToolDescriptor::new(
        "filesystem.search.grep",
        "search_grep",
        "Search UTF-8 filesystem files with a regular expression. Paths and results stay below the configured filesystem root.",
        json!({
            "type":"object", "additionalProperties":false, "required":["pattern"],
            "properties": {
                "pattern":{"type":"string", "minLength":1},
                "root":{"type":"string", "minLength":1},
                "glob":{"type":"array", "items":{"type":"string", "minLength":1}},
                "caseSensitive":{"type":"boolean"},
                "word":{"type":"boolean"},
                "contextLines":{"type":"integer", "minimum":0},
                "includeHidden":{"type":"boolean"},
                "maxDepth":{"type":"integer", "minimum":0},
                "maxResults":{"type":"integer", "minimum":1}
            }
        }),
    )
}

fn glob_descriptor() -> ToolDescriptor {
    ToolDescriptor::new(
        "filesystem.search.glob",
        "search_glob",
        "Find filesystem files using simple * and ? path patterns without following symlinks.",
        json!({
            "type":"object", "additionalProperties":false, "required":["patterns"],
            "properties": {
                "patterns":{"type":"array", "minItems":1, "items":{"type":"string", "minLength":1}},
                "root":{"type":"string", "minLength":1},
                "includeHidden":{"type":"boolean"},
                "maxDepth":{"type":"integer", "minimum":0},
                "maxResults":{"type":"integer", "minimum":1}
            }
        }),
    )
}

fn path_descriptor() -> ToolDescriptor {
    ToolDescriptor::new(
        "filesystem.search.path",
        "search_path",
        "Find filesystem files or directories using simple * and ? path patterns without following symlinks.",
        json!({
            "type":"object", "additionalProperties":false, "required":["patterns"],
            "properties": {
                "patterns":{"type":"array", "minItems":1, "items":{"type":"string", "minLength":1}},
                "root":{"type":"string", "minLength":1},
                "entryType":{"type":"string", "enum":["any", "file", "directory"]},
                "includeHidden":{"type":"boolean"},
                "maxDepth":{"type":"integer", "minimum":0},
                "maxResults":{"type":"integer", "minimum":1}
            }
        }),
    )
}

fn write_text_descriptor() -> ToolDescriptor {
    ToolDescriptor::new(
        "filesystem.write_text",
        "fs_write_text",
        "Create, overwrite, or append UTF-8 text below the configured filesystem root.",
        json!({
            "type":"object", "additionalProperties":false, "required":["path", "content"],
            "properties": {
                "path":{"type":"string", "minLength":1},
                "content":{"type":"string"},
                "mode":{"type":"string", "enum":["create", "overwrite", "append"]},
                "ensureParentDirs":{"type":"boolean"}
            }
        }),
    )
}

fn patch_descriptor() -> ToolDescriptor {
    ToolDescriptor::new(
        "filesystem.apply_patch",
        "fs_apply_patch",
        "Replace, delete, or insert text in one UTF-8 file below the configured filesystem root.",
        json!({
            "type":"object", "additionalProperties":false, "required":["path", "action", "target"],
            "properties": {
                "path":{"type":"string", "minLength":1},
                "action":{"type":"string", "enum":["replace", "delete", "insert_before", "insert_after"]},
                "target":{"type":"string", "minLength":1},
                "content":{"type":"string"},
                "replaceAll":{"type":"boolean"}
            }
        }),
    )
}

fn system_command_descriptor() -> ToolDescriptor {
    ToolDescriptor::new(
        "system.command.exec",
        "system_exec",
        "Run an executable with literal arguments. The Agent configuration can restrict executable names. It works on Linux, macOS, and Windows and is not a sandbox.",
        json!({
            "type":"object", "additionalProperties":false, "required":["command"],
            "properties": {
                "command":{"type":"string", "minLength":1},
                "args":{"type":"array", "items":{"type":"string"}},
                "cwd":{"type":"string", "minLength":1},
                "stdin":{"type":"string"},
                "env":{"type":"object", "additionalProperties":{"type":"string"}}
            }
        }),
    )
}

#[derive(Clone)]
struct ReadTextTool {
    filesystem: FileSystemRoot,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReadTextInput {
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
    max_bytes: Option<usize>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReadTextOutput {
    path: String,
    content: String,
    start_line: usize,
    end_line: usize,
    truncated: bool,
}

#[async_trait]
impl Tool for ReadTextTool {
    async fn call(&self, arguments: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        let input = parse(arguments)?;
        let filesystem = self.filesystem.clone();
        blocking(move || read_text(filesystem, input)).await
    }
}

fn read_text(filesystem: FileSystemRoot, input: ReadTextInput) -> Result<ToolOutput, ToolError> {
    let resolved = filesystem.resolve(&input.path)?;
    let file = File::open(&resolved.path)
        .map_err(|error| ToolError::Failed(format!("read `{}` failed: {error}", input.path)))?;
    let start_line = input.start_line.unwrap_or(1);
    if start_line == 0 {
        return Err(ToolError::Failed(
            "startLine must be greater than zero".into(),
        ));
    }
    let end_limit = input.end_line;
    if end_limit.is_some_and(|end_line| end_line < start_line) {
        return Err(ToolError::Failed(
            "endLine must be greater than or equal to startLine".into(),
        ));
    }
    let max_bytes = filesystem.max_read_bytes(input.max_bytes);
    let mut content = String::new();
    let mut truncated = false;
    let mut last_line = start_line.saturating_sub(1);
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line_number = index + 1;
        if line_number < start_line {
            continue;
        }
        if end_limit.is_some_and(|end_line| line_number > end_line) {
            break;
        }
        let line = line.map_err(|error| ToolError::Failed(format!("read text failed: {error}")))?;
        let prefix = (!content.is_empty()).then_some("\n");
        if prefix.is_some_and(|prefix| append_limited(&mut content, prefix, max_bytes))
            || append_limited(&mut content, &line, max_bytes)
        {
            truncated = true;
            break;
        }
        last_line = line_number;
    }
    json_output(ReadTextOutput {
        path: filesystem.display(&resolved.root, &resolved.path),
        content,
        start_line,
        end_line: end_limit.unwrap_or(last_line),
        truncated,
    })
}

#[derive(Clone)]
struct ListTool {
    filesystem: FileSystemRoot,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ListInput {
    path: String,
    recursive: Option<bool>,
    max_depth: Option<usize>,
    include_hidden: Option<bool>,
    files_only: Option<bool>,
    directories_only: Option<bool>,
    max_results: Option<usize>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ListOutput {
    path: String,
    entries: Vec<PathEntry>,
    truncated: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PathEntry {
    path: String,
    name: String,
    is_file: bool,
    is_directory: bool,
    is_symlink: bool,
    size_bytes: Option<u64>,
}

#[async_trait]
impl Tool for ListTool {
    async fn call(&self, arguments: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        let input = parse(arguments)?;
        let filesystem = self.filesystem.clone();
        blocking(move || list(filesystem, input)).await
    }
}

fn list(filesystem: FileSystemRoot, input: ListInput) -> Result<ToolOutput, ToolError> {
    if input.files_only.unwrap_or(false) && input.directories_only.unwrap_or(false) {
        return Err(ToolError::Failed(
            "filesOnly and directoriesOnly cannot both be true".into(),
        ));
    }
    let resolved = filesystem.resolve_directory(Some(&input.path))?;
    let mut entries = Vec::new();
    let max_results = filesystem.max_results(input.max_results);
    let recursive = input.recursive.unwrap_or(false);
    let max_depth = if recursive {
        filesystem.max_depth(input.max_depth)
    } else {
        0
    };
    let include_hidden = input.include_hidden.unwrap_or(false);
    let files_only = input.files_only.unwrap_or(false);
    let directories_only = input.directories_only.unwrap_or(false);
    let truncated = walk_entries(
        &resolved.path,
        0,
        max_depth,
        include_hidden,
        &mut |path, metadata| {
            let is_file = metadata.is_file();
            let is_directory = metadata.is_dir();
            if (files_only && !is_file) || (directories_only && !is_directory) {
                return false;
            }
            entries.push(PathEntry {
                path: filesystem.display(&resolved.root, path),
                name: path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                is_file,
                is_directory,
                is_symlink: metadata.file_type().is_symlink(),
                size_bytes: is_file.then_some(metadata.len()),
            });
            entries.len() >= max_results
        },
    )?;
    json_output(ListOutput {
        path: filesystem.display(&resolved.root, &resolved.path),
        entries,
        truncated,
    })
}

#[derive(Clone)]
struct StatTool {
    filesystem: FileSystemRoot,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StatInput {
    path: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatOutput {
    path: String,
    is_file: bool,
    is_directory: bool,
    is_symlink: bool,
    size_bytes: Option<u64>,
    readonly: bool,
}

#[async_trait]
impl Tool for StatTool {
    async fn call(&self, arguments: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        let input = parse(arguments)?;
        let filesystem = self.filesystem.clone();
        blocking(move || stat(filesystem, input)).await
    }
}

fn stat(filesystem: FileSystemRoot, input: StatInput) -> Result<ToolOutput, ToolError> {
    let resolved = filesystem.resolve(&input.path)?;
    let metadata = fs::symlink_metadata(&resolved.path)
        .map_err(|error| ToolError::Failed(format!("stat `{}` failed: {error}", input.path)))?;
    json_output(StatOutput {
        path: filesystem.display(&resolved.root, &resolved.path),
        is_file: metadata.is_file(),
        is_directory: metadata.is_dir(),
        is_symlink: metadata.file_type().is_symlink(),
        size_bytes: metadata.is_file().then_some(metadata.len()),
        readonly: metadata.permissions().readonly(),
    })
}

#[derive(Clone)]
struct GrepTool {
    filesystem: FileSystemRoot,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GrepInput {
    pattern: String,
    root: Option<String>,
    #[serde(default)]
    glob: Vec<String>,
    case_sensitive: Option<bool>,
    word: Option<bool>,
    context_lines: Option<usize>,
    include_hidden: Option<bool>,
    max_depth: Option<usize>,
    max_results: Option<usize>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GrepOutput {
    root: String,
    pattern: String,
    matches: Vec<GrepMatch>,
    truncated: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GrepMatch {
    path: String,
    line: usize,
    text: String,
    is_match: bool,
}

#[async_trait]
impl Tool for GrepTool {
    async fn call(&self, arguments: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        let input = parse(arguments)?;
        let filesystem = self.filesystem.clone();
        blocking(move || grep(filesystem, input)).await
    }
}

fn grep(filesystem: FileSystemRoot, input: GrepInput) -> Result<ToolOutput, ToolError> {
    let pattern = input.pattern.trim();
    if pattern.is_empty() {
        return Err(ToolError::Failed("pattern is required".into()));
    }
    let expression = if input.word.unwrap_or(false) {
        format!(r"\b(?:{pattern})\b")
    } else {
        pattern.to_owned()
    };
    let regex = RegexBuilder::new(&expression)
        .case_insensitive(!input.case_sensitive.unwrap_or(false))
        .build()
        .map_err(|error| ToolError::Failed(format!("invalid search pattern: {error}")))?;
    let root = filesystem.resolve_directory(input.root.as_deref())?;
    let max_results = filesystem.max_results(input.max_results);
    let context_lines = input.context_lines.unwrap_or(0);
    let include_hidden = input.include_hidden.unwrap_or(false);
    let max_depth = filesystem.max_depth(input.max_depth);
    let mut matches = Vec::new();
    let mut truncated = false;
    let mut read_error = None;
    let stopped = walk_entries(
        &root.path,
        0,
        max_depth,
        include_hidden,
        &mut |path, metadata| {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return false;
            }
            let relative = filesystem.display(&root.root, path);
            if !input.glob.is_empty()
                && !input
                    .glob
                    .iter()
                    .any(|glob| wildcard_matches(glob, &relative))
            {
                return false;
            }
            let lines = match read_lines_limited(path, filesystem.max_search_file_bytes()) {
                Ok(Some(lines)) => lines,
                Ok(None) => return false,
                Err(error) => {
                    read_error = Some(error);
                    return true;
                }
            };
            let matched = lines
                .iter()
                .enumerate()
                .filter_map(|(index, line)| regex.is_match(line).then_some(index))
                .collect::<Vec<_>>();
            let mut emitted = BTreeSet::new();
            for index in matched {
                let start = index.saturating_sub(context_lines);
                let end = (index + context_lines + 1).min(lines.len());
                for (line_index, line) in lines.iter().enumerate().take(end).skip(start) {
                    if !emitted.insert(line_index) {
                        continue;
                    }
                    matches.push(GrepMatch {
                        path: relative.clone(),
                        line: line_index + 1,
                        text: line.clone(),
                        is_match: regex.is_match(line),
                    });
                    if matches.len() >= max_results {
                        truncated = true;
                        return true;
                    }
                }
            }
            false
        },
    )?;
    if let Some(error) = read_error {
        return Err(error);
    }
    truncated |= stopped;
    json_output(GrepOutput {
        root: filesystem.display(&root.root, &root.path),
        pattern: pattern.to_owned(),
        matches,
        truncated,
    })
}

#[derive(Clone)]
struct GlobTool {
    filesystem: FileSystemRoot,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GlobInput {
    patterns: Vec<String>,
    root: Option<String>,
    include_hidden: Option<bool>,
    max_depth: Option<usize>,
    max_results: Option<usize>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GlobOutput {
    root: String,
    patterns: Vec<String>,
    paths: Vec<String>,
    truncated: bool,
}

#[async_trait]
impl Tool for GlobTool {
    async fn call(&self, arguments: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        let input = parse(arguments)?;
        let filesystem = self.filesystem.clone();
        blocking(move || glob(filesystem, input)).await
    }
}

fn glob(filesystem: FileSystemRoot, input: GlobInput) -> Result<ToolOutput, ToolError> {
    let patterns = normalize_patterns(input.patterns)?;
    let root = filesystem.resolve_directory(input.root.as_deref())?;
    let max_results = filesystem.max_results(input.max_results);
    let mut paths = Vec::new();
    let truncated = walk_entries(
        &root.path,
        0,
        filesystem.max_depth(input.max_depth),
        input.include_hidden.unwrap_or(false),
        &mut |path, metadata| {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return false;
            }
            let relative = filesystem.display(&root.root, path);
            if patterns
                .iter()
                .any(|pattern| wildcard_matches(pattern, &relative))
            {
                paths.push(relative);
            }
            paths.len() >= max_results
        },
    )?;
    json_output(GlobOutput {
        root: filesystem.display(&root.root, &root.path),
        patterns,
        paths,
        truncated,
    })
}

#[derive(Clone)]
struct PathTool {
    filesystem: FileSystemRoot,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PathInput {
    patterns: Vec<String>,
    root: Option<String>,
    entry_type: Option<PathEntryType>,
    include_hidden: Option<bool>,
    max_depth: Option<usize>,
    max_results: Option<usize>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PathEntryType {
    Any,
    File,
    Directory,
}

impl PathEntryType {
    fn matches(self, metadata: &fs::Metadata) -> bool {
        match self {
            Self::Any => true,
            Self::File => metadata.is_file(),
            Self::Directory => metadata.is_dir(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PathOutput {
    root: String,
    patterns: Vec<String>,
    entries: Vec<PathEntry>,
    truncated: bool,
}

#[async_trait]
impl Tool for PathTool {
    async fn call(&self, arguments: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        let input = parse(arguments)?;
        let filesystem = self.filesystem.clone();
        blocking(move || path_search(filesystem, input)).await
    }
}

fn path_search(filesystem: FileSystemRoot, input: PathInput) -> Result<ToolOutput, ToolError> {
    let patterns = normalize_patterns(input.patterns)?;
    let root = filesystem.resolve_directory(input.root.as_deref())?;
    let entry_type = input.entry_type.unwrap_or(PathEntryType::Any);
    let max_results = filesystem.max_results(input.max_results);
    let mut entries = Vec::new();
    let truncated = walk_entries(
        &root.path,
        0,
        filesystem.max_depth(input.max_depth),
        input.include_hidden.unwrap_or(false),
        &mut |path, metadata| {
            if metadata.file_type().is_symlink() || !entry_type.matches(metadata) {
                return false;
            }
            let relative = filesystem.display(&root.root, path);
            if !patterns
                .iter()
                .any(|pattern| wildcard_matches(pattern, &relative))
            {
                return false;
            }
            entries.push(PathEntry {
                path: relative,
                name: path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                is_file: metadata.is_file(),
                is_directory: metadata.is_dir(),
                is_symlink: false,
                size_bytes: metadata.is_file().then_some(metadata.len()),
            });
            entries.len() >= max_results
        },
    )?;
    json_output(PathOutput {
        root: filesystem.display(&root.root, &root.path),
        patterns,
        entries,
        truncated,
    })
}

#[derive(Clone)]
struct WriteTextTool {
    filesystem: FileSystemRoot,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WriteTextInput {
    path: String,
    content: String,
    mode: Option<WriteMode>,
    ensure_parent_dirs: Option<bool>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WriteMode {
    Create,
    Overwrite,
    Append,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WriteTextOutput {
    path: String,
    mode: &'static str,
    bytes_written: usize,
}

#[async_trait]
impl Tool for WriteTextTool {
    async fn call(&self, arguments: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        let input = parse(arguments)?;
        let filesystem = self.filesystem.clone();
        blocking(move || write_text(filesystem, input)).await
    }
}

fn write_text(filesystem: FileSystemRoot, input: WriteTextInput) -> Result<ToolOutput, ToolError> {
    let mut resolved = filesystem.resolve(&input.path)?;
    if input.ensure_parent_dirs.unwrap_or(true)
        && let Some(parent) = resolved.path.parent()
    {
        fs::create_dir_all(parent).map_err(|error| {
            ToolError::Failed(format!("create parent directories failed: {error}"))
        })?;
        resolved = filesystem.resolve(&input.path)?;
    }
    let mode = input.mode.unwrap_or(WriteMode::Overwrite);
    match mode {
        WriteMode::Create => OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&resolved.path)
            .and_then(|mut file| file.write_all(input.content.as_bytes())),
        WriteMode::Overwrite => fs::write(&resolved.path, input.content.as_bytes()),
        WriteMode::Append => OpenOptions::new()
            .create(true)
            .append(true)
            .open(&resolved.path)
            .and_then(|mut file| file.write_all(input.content.as_bytes())),
    }
    .map_err(|error| ToolError::Failed(format!("write `{}` failed: {error}", input.path)))?;
    json_output(WriteTextOutput {
        path: filesystem.display(&resolved.root, &resolved.path),
        mode: match mode {
            WriteMode::Create => "create",
            WriteMode::Overwrite => "overwrite",
            WriteMode::Append => "append",
        },
        bytes_written: input.content.len(),
    })
}

#[derive(Clone)]
struct PatchTool {
    filesystem: FileSystemRoot,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PatchInput {
    path: String,
    action: PatchAction,
    target: String,
    content: Option<String>,
    replace_all: Option<bool>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PatchAction {
    Replace,
    Delete,
    InsertBefore,
    InsertAfter,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PatchOutput {
    path: String,
    action: &'static str,
    replacements: usize,
}

#[async_trait]
impl Tool for PatchTool {
    async fn call(&self, arguments: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        let input = parse(arguments)?;
        let filesystem = self.filesystem.clone();
        blocking(move || patch(filesystem, input)).await
    }
}

fn patch(filesystem: FileSystemRoot, input: PatchInput) -> Result<ToolOutput, ToolError> {
    if input.target.is_empty() {
        return Err(ToolError::Failed("target is required".into()));
    }
    let resolved = filesystem.resolve(&input.path)?;
    let mut source = fs::read_to_string(&resolved.path)
        .map_err(|error| ToolError::Failed(format!("read `{}` failed: {error}", input.path)))?;
    let replacement = input.content.unwrap_or_default();
    let replace_all = input.replace_all.unwrap_or(false);
    let replacements = match input.action {
        PatchAction::Replace => {
            replace_text(&mut source, &input.target, &replacement, replace_all)?
        }
        PatchAction::Delete => replace_text(&mut source, &input.target, "", replace_all)?,
        PatchAction::InsertBefore => {
            insert_text(&mut source, &input.target, &replacement, replace_all, true)?
        }
        PatchAction::InsertAfter => {
            insert_text(&mut source, &input.target, &replacement, replace_all, false)?
        }
    };
    fs::write(&resolved.path, source.as_bytes())
        .map_err(|error| ToolError::Failed(format!("write `{}` failed: {error}", input.path)))?;
    json_output(PatchOutput {
        path: filesystem.display(&resolved.root, &resolved.path),
        action: match input.action {
            PatchAction::Replace => "replace",
            PatchAction::Delete => "delete",
            PatchAction::InsertBefore => "insert_before",
            PatchAction::InsertAfter => "insert_after",
        },
        replacements,
    })
}

struct SystemCommandTool {
    config: SystemCommandConfig,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SystemCommandInput {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    cwd: Option<String>,
    stdin: Option<String>,
    env: Option<std::collections::BTreeMap<String, String>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SystemCommandOutput {
    command: String,
    args: Vec<String>,
    cwd: Option<String>,
    success: bool,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

#[async_trait]
impl Tool for SystemCommandTool {
    async fn call(&self, arguments: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        let input = parse::<SystemCommandInput>(arguments)?;
        let command = input.command.trim().to_owned();
        if command.is_empty() {
            return Err(ToolError::Failed("command is required".into()));
        }
        if !self.config.allows(&command) {
            return Err(ToolError::Failed(format!(
                "command `{command}` is not permitted by this Agent configuration"
            )));
        }
        let cwd = input.cwd.as_deref().map(PathBuf::from);
        let mut process = tokio::process::Command::new(&command);
        process
            .args(&input.args)
            .kill_on_drop(true)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = &cwd {
            process.current_dir(cwd);
        }
        if let Some(env) = &input.env {
            process.envs(env);
        }
        if input.stdin.is_some() {
            process.stdin(Stdio::piped());
        } else {
            process.stdin(Stdio::null());
        }
        let mut child = process.spawn().map_err(|error| {
            ToolError::Failed(format!("command `{command}` could not start: {error}"))
        })?;
        if let Some(stdin) = input.stdin {
            let mut writer = child
                .stdin
                .take()
                .ok_or_else(|| ToolError::Failed("command stdin is not available".into()))?;
            writer.write_all(stdin.as_bytes()).await.map_err(|error| {
                ToolError::Failed(format!("command stdin write failed: {error}"))
            })?;
        }
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::Failed("command stdout is not available".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ToolError::Failed("command stderr is not available".into()))?;
        let max_output_bytes = self.config.max_output_bytes();
        let stdout_reader = tokio::spawn(read_output(stdout, max_output_bytes));
        let stderr_reader = tokio::spawn(read_output(stderr, max_output_bytes));
        let status = match tokio::time::timeout(self.config.timeout(), child.wait()).await {
            Ok(Ok(status)) => status,
            Ok(Err(error)) => {
                let _ = child.kill().await;
                let _ = stdout_reader.await;
                let _ = stderr_reader.await;
                return Err(ToolError::Indeterminate(format!(
                    "command `{command}` wait failed: {error}"
                )));
            }
            Err(_) => {
                let _ = child.kill().await;
                let _ = stdout_reader.await;
                let _ = stderr_reader.await;
                return Err(ToolError::Indeterminate(format!(
                    "command `{command}` exceeded the configured timeout"
                )));
            }
        };
        let (stdout, stdout_truncated) = join_reader(stdout_reader, "stdout").await?;
        let (stderr, stderr_truncated) = join_reader(stderr_reader, "stderr").await?;
        let output = SystemCommandOutput {
            command,
            args: input.args,
            cwd: cwd.map(|path| path.display().to_string()),
            success: status.success(),
            exit_code: status.code(),
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
        };
        json_output_with_error(output, !status.success())
    }
}

fn parse<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, ToolError> {
    serde_json::from_value(value)
        .map_err(|error| ToolError::Failed(format!("tool input is invalid: {error}")))
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ToolError> + Send + 'static,
) -> Result<T, ToolError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| ToolError::Failed(format!("filesystem tool task failed: {error}")))?
}

fn json_output<T: Serialize>(value: T) -> Result<ToolOutput, ToolError> {
    json_output_with_error(value, false)
}

fn json_output_with_error<T: Serialize>(value: T, is_error: bool) -> Result<ToolOutput, ToolError> {
    let structured = serde_json::to_value(&value)
        .map_err(|error| ToolError::Failed(format!("tool output serialization failed: {error}")))?;
    let content = serde_json::to_string(&structured)
        .map_err(|error| ToolError::Failed(format!("tool output serialization failed: {error}")))?;
    let mut output = if is_error {
        ToolOutput::error(content)
    } else {
        ToolOutput::text(content)
    };
    output.structured = Some(structured);
    Ok(output)
}

fn append_limited(target: &mut String, value: &str, limit: usize) -> bool {
    let remaining = limit.saturating_sub(target.len());
    if value.len() <= remaining {
        target.push_str(value);
        return false;
    }
    if remaining > 0 {
        let mut boundary = remaining.min(value.len());
        while boundary > 0 && !value.is_char_boundary(boundary) {
            boundary -= 1;
        }
        target.push_str(&value[..boundary]);
    }
    true
}

fn walk_entries(
    directory: &Path,
    depth: usize,
    max_depth: usize,
    include_hidden: bool,
    visitor: &mut dyn FnMut(&Path, &fs::Metadata) -> bool,
) -> Result<bool, ToolError> {
    let entries = fs::read_dir(directory).map_err(|error| {
        ToolError::Failed(format!("list `{}` failed: {error}", directory.display()))
    })?;
    for entry in entries {
        let entry =
            entry.map_err(|error| ToolError::Failed(format!("list entry failed: {error}")))?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if !include_hidden && name.starts_with('.') {
            continue;
        }
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            ToolError::Failed(format!("inspect `{}` failed: {error}", path.display()))
        })?;
        if visitor(&path, &metadata) {
            return Ok(true);
        }
        if metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && depth < max_depth
            && walk_entries(&path, depth + 1, max_depth, include_hidden, visitor)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn read_lines_limited(path: &Path, limit: usize) -> Result<Option<Vec<String>>, ToolError> {
    let mut bytes = Vec::with_capacity(limit.saturating_add(1));
    File::open(path)
        .map_err(|error| ToolError::Failed(format!("read `{}` failed: {error}", path.display())))?
        .take(limit.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| ToolError::Failed(format!("read `{}` failed: {error}", path.display())))?;
    if bytes.len() > limit {
        return Ok(None);
    }
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => return Ok(None),
    };
    Ok(Some(text.lines().map(ToOwned::to_owned).collect()))
}

fn normalize_patterns(patterns: Vec<String>) -> Result<Vec<String>, ToolError> {
    let patterns = patterns
        .into_iter()
        .map(|pattern| pattern.trim().to_owned())
        .filter(|pattern| !pattern.is_empty())
        .collect::<Vec<_>>();
    if patterns.is_empty() {
        return Err(ToolError::Failed(
            "patterns must contain at least one non-empty value".into(),
        ));
    }
    Ok(patterns)
}

fn wildcard_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.chars().collect::<Vec<_>>();
    let value = value.chars().collect::<Vec<_>>();
    let (mut pattern_index, mut value_index) = (0usize, 0usize);
    let mut star_index = None;
    let mut star_value_index = 0usize;
    while value_index < value.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == '?' || pattern[pattern_index] == value[value_index])
        {
            pattern_index += 1;
            value_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == '*' {
            star_index = Some(pattern_index);
            pattern_index += 1;
            star_value_index = value_index;
        } else if let Some(star) = star_index {
            pattern_index = star + 1;
            star_value_index += 1;
            value_index = star_value_index;
        } else {
            return false;
        }
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == '*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

fn replace_text(
    source: &mut String,
    target: &str,
    replacement: &str,
    replace_all: bool,
) -> Result<usize, ToolError> {
    let occurrences = source.matches(target).count();
    if occurrences == 0 {
        return Err(ToolError::Failed("target text was not found".into()));
    }
    *source = if replace_all {
        source.replace(target, replacement)
    } else {
        source.replacen(target, replacement, 1)
    };
    Ok(if replace_all { occurrences } else { 1 })
}

fn insert_text(
    source: &mut String,
    target: &str,
    content: &str,
    replace_all: bool,
    before: bool,
) -> Result<usize, ToolError> {
    let replacement = if before {
        format!("{content}{target}")
    } else {
        format!("{target}{content}")
    };
    replace_text(source, target, &replacement, replace_all)
}

async fn read_output<R>(mut reader: R, limit: usize) -> Result<(String, bool), ToolError>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::with_capacity(limit.min(8192));
    let mut buffer = [0u8; 8192];
    let mut truncated = false;
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|error| ToolError::Failed(format!("command output read failed: {error}")))?;
        if read == 0 {
            break;
        }
        let available = limit.saturating_sub(bytes.len());
        let retained = read.min(available);
        bytes.extend_from_slice(&buffer[..retained]);
        truncated |= retained < read;
    }
    Ok((String::from_utf8_lossy(&bytes).to_string(), truncated))
}

async fn join_reader(
    task: tokio::task::JoinHandle<Result<(String, bool), ToolError>>,
    stream: &str,
) -> Result<(String, bool), ToolError> {
    task.await
        .map_err(|error| ToolError::Failed(format!("command {stream} reader failed: {error}")))?
}
