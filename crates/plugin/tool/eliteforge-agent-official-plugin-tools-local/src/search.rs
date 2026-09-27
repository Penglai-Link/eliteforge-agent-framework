//! Deterministic ignore-aware scans with bounded, session-scoped result snapshots.
use std::{
    collections::{BTreeSet, VecDeque},
    fs,
    io::Read,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use eliteforge_agent_core::{RunContext, Tool, ToolBinding, ToolDescriptor, ToolError, ToolOutput};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use regex::RegexBuilder;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    edits,
    filesystem::FileSystemRoot,
    tools::{blocking, json_output, parse},
};

const MAX_ENTRIES: usize = 10000;
const MAX_SNAPSHOT_BYTES: usize = 2 * 1024 * 1024;
const MAX_SCAN_BYTES: usize = 32 * 1024 * 1024;
const TTL: Duration = Duration::from_secs(120);

#[derive(Debug, Default)]
pub(crate) struct SearchCache {
    snapshots: VecDeque<Snapshot>,
}

#[derive(Debug)]
struct Snapshot {
    id: String,
    session: String,
    query: Value,
    created: Instant,
    output: Value,
    rows: Vec<Value>,
    key: &'static str,
}

#[derive(Clone, Copy)]
enum Kind {
    Grep,
    Glob,
    Path,
}

pub(crate) fn bindings(filesystem: FileSystemRoot) -> Vec<ToolBinding> {
    [(Kind::Grep, "grep"), (Kind::Glob, "glob"), (Kind::Path, "path")].into_iter().map(|(kind, name)| {
        let mut properties = json!({
            "root":{"type":"string","minLength":1},
            "exclude":{"type":"array","maxItems":64,"items":{"type":"string","minLength":1}},
            "respectGitIgnore":{"type":"boolean"},
            "includeHidden":{"type":"boolean"},
            "maxDepth":{"type":"integer","minimum":0},
            "maxResults":{"type":"integer","minimum":1},
            "cursor":{"type":"string","minLength":1,"maxLength":128}
        });
        let required = if matches!(kind, Kind::Grep) {
            properties.as_object_mut().unwrap().extend(json!({
                "pattern":{"type":"string","minLength":1,"maxLength":8192},
                "glob":{"type":"array","maxItems":64,"items":{"type":"string","minLength":1}},
                "caseSensitive":{"type":"boolean"},"word":{"type":"boolean"},
                "fixedStrings":{"type":"boolean"},
                "contextLines":{"type":"integer","minimum":0,"maximum":100},
                "mode":{"type":"string","enum":["content","files","count"]}
            }).as_object().unwrap().clone());
            "pattern"
        } else {
            properties["patterns"] = json!({"type":"array","minItems":1,"maxItems":64,"items":{"type":"string","minLength":1}});
            if matches!(kind, Kind::Path) { properties["entryType"] = json!({"type":"string","enum":["any","file","directory"]}); }
            "patterns"
        };
        ToolBinding::new(ToolDescriptor::new(format!("filesystem.search.{name}"), format!("search_{name}"),
            "Search below the configured root using full glob syntax (**/braces/classes), excludes and .gitignore/.ignore rules. Results use a bounded snapshot: repeat the same query with nextCursor within 120 seconds for stable pages. Inspect complete, skipped and limitReason before treating a scan as exhaustive.",
            json!({"type":"object","additionalProperties":false,"required":[required],"properties":properties})),
            SearchTool {filesystem:filesystem.clone(),kind}).parallel_safe()
    }).collect()
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Input {
    pattern: Option<String>,
    #[serde(default)]
    patterns: Vec<String>,
    #[serde(default)]
    glob: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
    root: Option<String>,
    respect_git_ignore: Option<bool>,
    #[serde(default)]
    include_hidden: bool,
    max_depth: Option<usize>,
    max_results: Option<usize>,
    cursor: Option<String>,
    #[serde(default)]
    case_sensitive: bool,
    #[serde(default)]
    word: bool,
    #[serde(default)]
    fixed_strings: bool,
    #[serde(default)]
    context_lines: usize,
    mode: Option<Mode>,
    entry_type: Option<EntryType>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Mode {
    Content,
    Files,
    Count,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum EntryType {
    Any,
    File,
    Directory,
}

struct SearchTool {
    filesystem: FileSystemRoot,
    kind: Kind,
}

#[async_trait]
impl Tool for SearchTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let input: Input = parse(arguments.clone())?;
        edits::ensure_active(context)?;
        let mut query = arguments;
        let object = query
            .as_object_mut()
            .ok_or_else(|| failed("expected object"))?;
        object.remove("cursor");
        object.remove("maxResults");
        object.insert(
            "tool".into(),
            json!(match self.kind {
                Kind::Grep => "grep",
                Kind::Glob => "glob",
                Kind::Path => "path",
            }),
        );
        let limit = self.filesystem.max_results(input.max_results);
        if let Some(cursor) = &input.cursor {
            let (id, offset) = cursor
                .split_once(':')
                .ok_or_else(|| failed("invalid cursor"))?;
            let offset: usize = offset
                .parse()
                .map_err(|_| failed("invalid cursor offset"))?;
            let mut cache = self
                .filesystem
                .searches
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            cache.snapshots.retain(|s| s.created.elapsed() < TTL);
            let snapshot = cache.snapshots.iter().find(|s| s.id == id && s.session == context.session_id && s.query == query)
                .ok_or_else(|| failed("cursor expired, evicted or belongs to another query/session; restart the search"))?;
            return page(snapshot, offset, limit);
        }
        let filesystem = self.filesystem.clone();
        let context = context.clone();
        let kind = self.kind;
        blocking(move || {
            let mut snapshot = scan(&filesystem, &input, kind, &context)?;
            edits::ensure_active(&context)?;
            snapshot.query = query;
            let output = page(&snapshot, 0, limit)?;
            // A single page needs no cache. Multi-page snapshots are capped at
            // eight entries (16 MiB of serialized rows) per plugin installation.
            if snapshot.rows.len() > limit {
                let mut cache = filesystem
                    .searches
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                cache.snapshots.retain(|s| s.created.elapsed() < TTL);
                while cache.snapshots.len() >= 8 {
                    cache.snapshots.pop_front();
                }
                cache.snapshots.push_back(snapshot);
            }
            Ok(output)
        })
        .await
    }
}

fn page(snapshot: &Snapshot, offset: usize, limit: usize) -> Result<ToolOutput, ToolError> {
    if offset > snapshot.rows.len() {
        return Err(failed("cursor offset outside snapshot"));
    }
    let end = offset.saturating_add(limit).min(snapshot.rows.len());
    let mut output = snapshot.output.clone();
    output[snapshot.key] = json!(&snapshot.rows[offset..end]);
    output["nextCursor"] = if end < snapshot.rows.len() {
        json!(format!("{}:{end}", snapshot.id))
    } else {
        Value::Null
    };
    output["truncated"] = json!(end < snapshot.rows.len() || output["complete"] != true);
    output["snapshot"] = json!(snapshot.id);
    output["offset"] = json!(offset);
    json_output(output)
}

fn globset(patterns: &[String]) -> Result<GlobSet, ToolError> {
    if patterns.len() > 64 {
        return Err(failed("at most 64 glob patterns"));
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        if pattern.is_empty() || pattern.len() > 8192 {
            return Err(failed("invalid glob length"));
        }
        builder.add(
            GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .map_err(|error| failed(format!("invalid glob: {error}")))?,
        );
        // Like command-line file search, a basename glob applies at every depth.
        // Patterns containing a separator remain relative to the selected root.
        if !pattern.contains('/') && !pattern.contains('\\') {
            builder.add(
                GlobBuilder::new(&format!("**/{pattern}"))
                    .literal_separator(true)
                    .build()
                    .map_err(|error| failed(format!("invalid glob: {error}")))?,
            );
        }
    }
    builder.build().map_err(|error| failed(error.to_string()))
}

fn scan(
    filesystem: &FileSystemRoot,
    input: &Input,
    kind: Kind,
    context: &RunContext,
) -> Result<Snapshot, ToolError> {
    edits::ensure_active(context)?;
    if input.context_lines > 100 {
        return Err(failed("contextLines exceeds 100"));
    }
    let root = filesystem.resolve_directory(input.root.as_deref())?;
    let mode = input.mode.unwrap_or(Mode::Content);
    let patterns = if matches!(kind, Kind::Grep) {
        &input.glob
    } else {
        &input.patterns
    };
    if !matches!(kind, Kind::Grep) && patterns.is_empty() {
        return Err(failed("patterns cannot be empty"));
    }
    let includes = globset(patterns)?;
    let excludes = globset(&input.exclude)?;
    let regex = if matches!(kind, Kind::Grep) {
        let pattern = input
            .pattern
            .as_deref()
            .ok_or_else(|| failed("pattern is required"))?;
        if pattern.is_empty() || pattern.len() > 8192 {
            return Err(failed("invalid pattern length"));
        }
        let pattern = if input.fixed_strings {
            regex::escape(pattern)
        } else {
            pattern.to_owned()
        };
        let pattern = if input.word {
            format!(r"\b(?:{pattern})\b")
        } else {
            pattern
        };
        Some(
            RegexBuilder::new(&pattern)
                .case_insensitive(!input.case_sensitive)
                .size_limit(2 * 1024 * 1024)
                .build()
                .map_err(|e| failed(format!("invalid pattern: {e}")))?,
        )
    } else {
        None
    };
    let mut builder = ignore::WalkBuilder::new(&root.path);
    let use_ignore = input.respect_git_ignore.unwrap_or(true);
    builder
        .hidden(!input.include_hidden)
        .ignore(use_ignore)
        .git_ignore(use_ignore)
        .git_exclude(use_ignore)
        .git_global(false)
        .parents(false)
        .require_git(false)
        .follow_links(false)
        .max_depth(Some(
            filesystem.max_depth(input.max_depth).saturating_add(1),
        ));
    let excluded = excludes.clone();
    let configured_root = root.root.clone();
    builder.filter_entry(move |entry| {
        entry.depth() == 0
            || !excluded.is_match(
                entry
                    .path()
                    .strip_prefix(&configured_root)
                    .unwrap_or(entry.path()),
            )
    });
    let started = Instant::now();
    let mut rows = Vec::new();
    let mut result_bytes = 0usize;
    let mut scan_bytes = 0usize;
    let mut scanned = 0usize;
    let mut skipped = 0usize;
    let mut reason = None;
    'scan: for entry in builder.build() {
        edits::ensure_active(context)?;
        if started.elapsed() > Duration::from_secs(5) {
            reason = Some("scan_timeout");
            break;
        }
        if scanned >= MAX_ENTRIES {
            reason = Some("entry_limit");
            break;
        }
        let entry = entry.map_err(|e| failed(format!("walk failed: {e}")))?;
        if entry.depth() == 0 {
            continue;
        }
        scanned += 1;
        if entry.error().is_some() {
            return Err(failed("invalid ignore rules encountered"));
        }
        let metadata = fs::symlink_metadata(entry.path()).map_err(|e| failed(e.to_string()))?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        let relative = filesystem.display(&root.root, entry.path());
        if excludes.is_match(&relative) || (!patterns.is_empty() && !includes.is_match(&relative)) {
            continue;
        }
        // Revalidate after walking. This rejects symlink replacements before read.
        let path = filesystem.resolve(&relative)?.path;
        let mut candidates = Vec::new();
        match kind {
            Kind::Glob if metadata.is_file() => candidates.push(json!(relative)),
            Kind::Path => {
                let accepts = match input.entry_type.unwrap_or(EntryType::Any) {
                    EntryType::Any => true,
                    EntryType::File => metadata.is_file(),
                    EntryType::Directory => metadata.is_dir(),
                };
                if accepts {
                    candidates.push(json!({"path":relative,"name":entry.file_name().to_string_lossy(),"isFile":metadata.is_file(),"isDirectory":metadata.is_dir(),"isSymlink":false,"sizeBytes":metadata.is_file().then_some(metadata.len())}));
                }
            }
            Kind::Grep if metadata.is_file() => {
                if metadata.len() > filesystem.max_search_file_bytes() as u64 {
                    skipped += 1;
                    continue;
                }
                let read_limit = filesystem
                    .max_search_file_bytes()
                    .min(MAX_SCAN_BYTES.saturating_sub(scan_bytes));
                let mut bytes = Vec::new();
                fs::File::open(path)
                    .map_err(|e| failed(e.to_string()))?
                    .take(read_limit.saturating_add(1) as u64)
                    .read_to_end(&mut bytes)
                    .map_err(|e| failed(e.to_string()))?;
                scan_bytes = scan_bytes.saturating_add(bytes.len());
                if scan_bytes > MAX_SCAN_BYTES {
                    reason = Some("scan_bytes_limit");
                    break;
                }
                if bytes.len() > read_limit {
                    skipped += 1;
                    continue;
                }
                let Ok(text) = edits::text(&bytes) else {
                    skipped += 1;
                    continue;
                };
                let regex = regex.as_ref().unwrap();
                let lines: Vec<_> = text.lines().collect();
                let mut matching = BTreeSet::new();
                for (index, line) in lines.iter().enumerate() {
                    edits::ensure_active(context)?;
                    if regex.is_match(line) {
                        matching.insert(index);
                    }
                }
                if matching.is_empty() {
                    continue;
                }
                match mode {
                    Mode::Files => candidates.push(json!(relative)),
                    Mode::Count => candidates.push(json!({"path":relative,"count":matching.len()})),
                    Mode::Content => {
                        let mut emitted = BTreeSet::new();
                        for index in &matching {
                            for (row, line) in lines
                                .iter()
                                .enumerate()
                                .take((index + input.context_lines + 1).min(lines.len()))
                                .skip(index.saturating_sub(input.context_lines))
                            {
                                if !emitted.insert(row) {
                                    continue;
                                }
                                let mut end = line.len().min(8192);
                                while !line.is_char_boundary(end) {
                                    end -= 1;
                                }
                                candidates.push(json!({"path":relative,"line":row+1,"text":&line[..end],"textTruncated":end<line.len(),"isMatch":matching.contains(&row)}));
                                // Bound transient candidate storage, including a file
                                // consisting of thousands of matching empty lines.
                                if candidates.len() >= MAX_ENTRIES {
                                    break;
                                }
                            }
                            if candidates.len() >= MAX_ENTRIES {
                                break;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        for row in candidates {
            let bytes = serde_json::to_vec(&row)
                .map_err(|e| failed(e.to_string()))?
                .len();
            if rows.len() >= MAX_ENTRIES || result_bytes.saturating_add(bytes) > MAX_SNAPSHOT_BYTES
            {
                reason = Some("result_limit");
                break 'scan;
            }
            result_bytes += bytes;
            rows.push(row);
        }
        if rows.len() >= MAX_ENTRIES {
            reason = Some("result_limit");
            break;
        }
    }
    rows.sort_by(|a, b| {
        fn path(value: &Value) -> &str {
            value
                .as_str()
                .or_else(|| value.get("path").and_then(Value::as_str))
                .unwrap_or_default()
        }
        path(a).cmp(path(b)).then_with(|| {
            a.get("line")
                .and_then(Value::as_u64)
                .cmp(&b.get("line").and_then(Value::as_u64))
        })
    });
    let key = match kind {
        Kind::Glob => "paths",
        Kind::Path => "entries",
        Kind::Grep => match mode {
            Mode::Content => "matches",
            Mode::Files => "paths",
            Mode::Count => "counts",
        },
    };
    Ok(Snapshot {
        id: uuid::Uuid::new_v4().simple().to_string(),
        session: context.session_id.clone(),
        query: Value::Null,
        created: Instant::now(),
        rows,
        key,
        output: json!({"root":filesystem.display(&root.root,&root.path),"pattern":input.pattern,"patterns":input.patterns,
            "complete":reason.is_none() && skipped==0,"limitReason":reason,"scannedEntries":scanned,"skipped":skipped,
            "maxDepth":filesystem.max_depth(input.max_depth),"respectGitIgnore":use_ignore}),
    })
}

fn failed(message: impl Into<String>) -> ToolError {
    ToolError::Failed(message.into())
}

#[cfg(test)]
mod basename_glob_tests {
    use super::globset;

    #[test]
    fn basename_patterns_match_nested_files_but_paths_remain_root_relative() {
        let names = globset(&["*.rs".into()]).unwrap();
        assert!(names.is_match("main.rs"));
        assert!(names.is_match("src/nested/main.rs"));
        assert!(!names.is_match("src/main.ts"));
        let paths = globset(&["src/*.rs".into()]).unwrap();
        assert!(paths.is_match("src/main.rs"));
        assert!(!paths.is_match("other/src/main.rs"));
        assert!(!paths.is_match("src/nested/main.rs"));
    }
}
