//! Bounded text snapshots and staged multi-file edits. The lock coordinates local
//! tools in this process; external programs must still cooperate with the host.
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::Path,
    sync::{Mutex, MutexGuard},
};

use async_trait::async_trait;
use diffy::patch_set::{FileMode, FileOperation, ParseOptions, PatchSet};
use eliteforge_agent_core::{RunContext, Tool, ToolBinding, ToolDescriptor, ToolError, ToolOutput};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::{
    filesystem::FileSystemRoot,
    tools::{blocking, json_output, parse},
};

static EDIT_LOCK: Mutex<()> = Mutex::new(());
const MAX_PATCH_BYTES: usize = 2 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 32 * 1024 * 1024;

pub(crate) fn lock() -> MutexGuard<'static, ()> {
    EDIT_LOCK.lock().unwrap_or_else(|error| error.into_inner())
}

pub(crate) fn version(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

pub(crate) fn read(path: &Path, limit: usize) -> Result<Option<Vec<u8>>, ToolError> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(failed(format!("read {}: {error}", path.display()))),
    };
    if !file.metadata().map_err(io_error)?.is_file() {
        return Err(failed("only regular text files are supported"));
    }
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() > limit {
        return Err(failed("file exceeds max_edit_bytes"));
    }
    Ok(Some(bytes))
}

pub(crate) fn check_version(bytes: Option<&[u8]>, expected: Option<&str>) -> Result<(), ToolError> {
    if let Some(expected) = expected {
        let actual = bytes.map(version).unwrap_or_else(|| "absent".into());
        if actual != expected {
            return Err(failed(format!(
                "version conflict: expected {expected}, found {actual}; read the file again"
            )));
        }
    }
    Ok(())
}

pub(crate) fn text(bytes: &[u8]) -> Result<&str, ToolError> {
    let text = std::str::from_utf8(bytes).map_err(|_| failed("file is not UTF-8 text"))?;
    if text.contains('\0') {
        return Err(failed("binary files are not supported"));
    }
    Ok(text)
}

pub(crate) fn binding(filesystem: FileSystemRoot) -> ToolBinding {
    ToolBinding::new(
        ToolDescriptor::new(
            "filesystem.apply_unified_patch",
            "fs_apply_unified_patch",
            "Apply a bounded multi-file unified diff (create/modify/delete/rename). Every affected root-relative path requires expectedVersions from fs_read_text, or 'absent' for creation. dryRun validates and previews without writing. Stages all files and rolls back on commit failure; external writers must cooperate.",
            json!({"type":"object","additionalProperties":false,"required":["patch","expectedVersions"],"properties":{
                "patch":{"type":"string","minLength":1},
                "format":{"type":"string","enum":["unified","git"]},
                "strip":{"type":"integer","minimum":0,"maximum":8},
                "expectedVersions":{"type":"object","additionalProperties":{"type":"string","minLength":1}},
                "dryRun":{"type":"boolean"}
            }}),
        ).with_display_resource(include_str!("tool-display.json")).with_single_input().with_operation_kind(eliteforge_agent_core::ToolOperationKind::Command),
        UnifiedPatchTool { filesystem },
    )
}

struct UnifiedPatchTool {
    filesystem: FileSystemRoot,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PatchInput {
    patch: String,
    format: Option<PatchFormat>,
    strip: Option<usize>,
    expected_versions: BTreeMap<String, String>,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PatchFormat {
    Unified,
    Git,
}

#[async_trait]
impl Tool for UnifiedPatchTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let input = parse(arguments)?;
        let filesystem = self.filesystem.clone();
        let context = context.clone();
        blocking(move || apply_patch(&filesystem, input, &context)).await
    }
}

pub(crate) struct Change {
    pub path: String,
    pub before: Option<Vec<u8>>,
    pub after: Option<Vec<u8>>,
    pub permissions: Option<fs::Permissions>,
}

fn apply_patch(
    filesystem: &FileSystemRoot,
    input: PatchInput,
    context: &RunContext,
) -> Result<ToolOutput, ToolError> {
    let _guard = lock();
    ensure_active(context)?;
    if input.patch.len() > MAX_PATCH_BYTES || input.strip.is_some_and(|n| n > 8) {
        return Err(failed("patch exceeds input limits"));
    }
    let format = input.format.unwrap_or(PatchFormat::Unified);
    let options = match format {
        PatchFormat::Unified => ParseOptions::unidiff(),
        PatchFormat::Git => ParseOptions::gitdiff(),
    };
    let strip = input.strip.unwrap_or(match format {
        PatchFormat::Unified => 0,
        PatchFormat::Git => 1,
    });
    // Normalize version keys through the same authorized root resolution as
    // patch paths. Hosts with multiple roots display absolute paths, while
    // callers may still supply the documented relative paths.
    let mut expected_versions = std::collections::HashMap::new();
    for (raw, version) in &input.expected_versions {
        let resolved = filesystem.resolve(raw)?;
        let path = filesystem.display(&resolved.root, &resolved.path);
        if expected_versions.insert(path, version).is_some() {
            return Err(failed("expectedVersions contains duplicate paths"));
        }
    }
    let mut changes = Vec::new();
    let mut total = 0usize;
    for patch in PatchSet::parse(&input.patch, options) {
        ensure_active(context)?;
        let patch = patch.map_err(|error| failed(format!("invalid patch: {error}")))?;
        if changes.len() >= 32 {
            return Err(failed("at most 32 files per patch"));
        }
        if patch
            .old_mode()
            .into_iter()
            .chain(patch.new_mode())
            .any(|mode| !matches!(mode, FileMode::Regular))
        {
            return Err(failed(
                "mode changes, executable creation, symlinks and submodules are not supported",
            ));
        }
        let rename_header = matches!(patch.operation(), FileOperation::Rename { .. });
        let operation = patch
            .operation()
            .strip_prefix(if rename_header { 0 } else { strip });
        let (raw, destination, create, delete) = match &operation {
            FileOperation::Create(path) => (path.as_ref(), None, true, false),
            FileOperation::Delete(path) => (path.as_ref(), None, false, true),
            FileOperation::Modify { original, modified } => (
                original.as_ref(),
                (original != modified).then_some(modified.as_ref()),
                false,
                false,
            ),
            FileOperation::Rename { from, to } => (from.as_ref(), Some(to.as_ref()), false, false),
            FileOperation::Copy { .. } => {
                return Err(failed("copy headers are not supported; use a create patch"));
            }
        };
        let resolved = filesystem.resolve(raw)?;
        let path = filesystem.display(&resolved.root, &resolved.path);
        if changes.iter().any(|change: &Change| change.path == path) {
            return Err(failed("duplicate patch path"));
        }
        let expected = expected_versions
            .get(&path)
            .ok_or_else(|| failed(format!("expectedVersions missing {path}")))?;
        let before = read(&resolved.path, filesystem.max_edit_bytes())?;
        check_version(before.as_deref(), Some(expected))?;
        if create != before.is_none() {
            return Err(failed(format!("patch file existence mismatch: {path}")));
        }
        let destination = if let Some(destination) = destination {
            let target = filesystem.resolve(destination)?;
            let name = filesystem.display(&target.root, &target.path);
            if name == path || changes.iter().any(|c: &Change| c.path == name) {
                return Err(failed("duplicate rename path"));
            }
            let expected = expected_versions
                .get(&name)
                .ok_or_else(|| failed(format!("expectedVersions missing {name}")))?;
            let existing = read(&target.path, filesystem.max_edit_bytes())?;
            check_version(existing.as_deref(), Some(expected))?;
            if existing.is_some() {
                return Err(failed("rename destination must be absent"));
            }
            Some(name)
        } else {
            None
        };
        let original = text(before.as_deref().unwrap_or_default())?;
        let patch = patch
            .patch()
            .as_text()
            .ok_or_else(|| failed("binary patches are not supported"))?;
        let after = diffy::apply(original, patch)
            .map_err(|error| failed(format!("patch conflict in {path}: {error}")))?;
        if delete && !after.is_empty() {
            return Err(failed("delete patch must remove all file content"));
        }
        if after.len() > filesystem.max_edit_bytes() {
            return Err(failed("patched file exceeds max_edit_bytes"));
        }
        total = total
            .saturating_add(original.len())
            .saturating_add(after.len());
        if total > MAX_TOTAL_BYTES {
            return Err(failed("patch snapshots exceed 32 MiB"));
        }
        if let Some(destination) = destination {
            // Both paths participate in the same validated/staged transaction.
            let permissions = Some(
                fs::metadata(&resolved.path)
                    .map_err(io_error)?
                    .permissions(),
            );
            changes.push(Change {
                path,
                before,
                after: None,
                permissions: None,
            });
            changes.push(Change {
                path: destination,
                before: None,
                after: Some(after.into_bytes()),
                permissions,
            });
        } else {
            changes.push(Change {
                path,
                before,
                after: (!delete).then(|| after.into_bytes()),
                permissions: None,
            });
        }
        if changes.len() > 32 {
            return Err(failed("at most 32 affected paths per patch"));
        }
    }
    if changes.is_empty() {
        return Err(failed("patch contains no file changes"));
    }
    if changes.len() != expected_versions.len() {
        return Err(failed(
            "expectedVersions contains paths not modified by this patch",
        ));
    }
    let previews = changes.iter().map(preview).collect::<Result<Vec<_>, _>>()?;
    if !input.dry_run {
        let _directories = prepare_parents(filesystem, &changes)?;
        commit(filesystem, &changes, Some(context))?;
    }
    json_output(json!({"dryRun": input.dry_run, "applied": !input.dry_run, "changes": previews}))
}

pub(crate) struct CreatedDirectories(Vec<std::path::PathBuf>);
impl Drop for CreatedDirectories {
    fn drop(&mut self) {
        // Remove only empty directories made by this call. Successful edits and
        // external files keep their parent directories intact.
        for path in self.0.iter().rev() {
            let _ = fs::remove_dir(path);
        }
    }
}

pub(crate) fn prepare_parents(
    filesystem: &FileSystemRoot,
    changes: &[Change],
) -> Result<CreatedDirectories, ToolError> {
    let mut created = CreatedDirectories(Vec::new());
    for change in changes.iter().filter(|c| c.after.is_some()) {
        let resolved = filesystem.resolve(&change.path)?;
        let parent = resolved
            .path
            .parent()
            .ok_or_else(|| failed("missing parent directory"))?;
        let relative = parent
            .strip_prefix(&resolved.root)
            .map_err(|_| failed("parent outside root"))?;
        let mut current = resolved.root.clone();
        for component in relative.components() {
            current.push(component);
            let validated = filesystem.resolve(&filesystem.display(&resolved.root, &current))?;
            match fs::create_dir(&validated.path) {
                Ok(()) => created.0.push(validated.path),
                Err(error)
                    if error.kind() == std::io::ErrorKind::AlreadyExists
                        && validated.path.is_dir() => {}
                Err(error) => return Err(io_error(error)),
            }
        }
    }
    Ok(created)
}

pub(crate) fn preview(change: &Change) -> Result<Value, ToolError> {
    let before = text(change.before.as_deref().unwrap_or_default())?;
    let after = text(change.after.as_deref().unwrap_or_default())?;
    let mut diff = diffy::create_patch(before, after).to_string();
    let truncated = diff.len() > 16384;
    if truncated {
        let mut end = 16384;
        while !diff.is_char_boundary(end) {
            end -= 1;
        }
        diff.truncate(end);
    }
    Ok(json!({"path":change.path,
        "previousVersion": change.before.as_deref().map(version).unwrap_or_else(|| "absent".into()),
        "version": change.after.as_deref().map(version).unwrap_or_else(|| "absent".into()),
        "diff":diff,"diffTruncated":truncated}))
}

/// Caller holds EDIT_LOCK. Stage in each destination directory, then recheck all
/// versions. A later failure restores earlier changes while preserving file modes.
pub(crate) fn commit(
    filesystem: &FileSystemRoot,
    changes: &[Change],
    context: Option<&RunContext>,
) -> Result<(), ToolError> {
    commit_with(filesystem, changes, context, |_| {})
}

fn commit_with(
    filesystem: &FileSystemRoot,
    changes: &[Change],
    context: Option<&RunContext>,
    mut before_write: impl FnMut(usize),
) -> Result<(), ToolError> {
    let mut staged = Vec::new();
    let mut permissions = Vec::new();
    for change in changes {
        if let Some(context) = context {
            ensure_active(context)?;
        }
        let resolved = filesystem.resolve(&change.path)?;
        let mode = fs::metadata(&resolved.path).ok().map(|m| m.permissions());
        permissions.push(mode.clone());
        staged.push(match &change.after {
            Some(bytes) => Some(stage(
                &resolved.path,
                bytes,
                change.permissions.clone().or(mode),
            )?),
            None => None,
        });
    }
    for change in changes {
        verify_unchanged(filesystem, change, &change.before)?;
    }
    for (index, change) in changes.iter().enumerate() {
        before_write(index);
        let result = (|| {
            if let Some(context) = context {
                ensure_active(context)?;
            }
            verify_unchanged(filesystem, change, &change.before)?;
            let resolved = filesystem.resolve(&change.path)?;
            match staged[index].take() {
                Some(file) if change.before.is_none() => file
                    .persist_noclobber(&resolved.path)
                    .map(|_| ())
                    .map_err(|e| io_error(e.error)),
                Some(file) => file
                    .persist(&resolved.path)
                    .map(|_| ())
                    .map_err(|e| io_error(e.error)),
                None => fs::remove_file(&resolved.path).map_err(io_error),
            }
        })();
        if let Err(error) = result {
            let mut failures = Vec::new();
            for undo_index in (0..index).rev() {
                let undo = &changes[undo_index];
                let result = (|| {
                    verify_unchanged(filesystem, undo, &undo.after)?;
                    let resolved = filesystem.resolve(&undo.path)?;
                    match &undo.before {
                        Some(bytes) => {
                            stage(&resolved.path, bytes, permissions[undo_index].clone())?
                                .persist(&resolved.path)
                                .map(|_| ())
                                .map_err(|e| io_error(e.error))
                        }
                        None => fs::remove_file(&resolved.path).map_err(io_error),
                    }
                })();
                if let Err(error) = result {
                    failures.push(format!("{}: {error}", undo.path));
                }
            }
            if failures.is_empty() {
                return Err(failed(format!(
                    "edit failed; earlier file changes rolled back: {error}"
                )));
            }
            return Err(ToolError::Indeterminate(format!(
                "edit failed: {error}; rollback incomplete: {}",
                failures.join("; ")
            )));
        }
    }
    Ok(())
}

fn verify_unchanged(
    filesystem: &FileSystemRoot,
    change: &Change,
    expected: &Option<Vec<u8>>,
) -> Result<(), ToolError> {
    let path = filesystem.resolve(&change.path)?.path;
    let actual = read(&path, filesystem.max_edit_bytes())?;
    if actual != *expected {
        return Err(failed(format!(
            "version conflict during commit: {}",
            change.path
        )));
    }
    Ok(())
}

fn stage(
    path: &Path,
    bytes: &[u8],
    permissions: Option<fs::Permissions>,
) -> Result<NamedTempFile, ToolError> {
    let parent = path
        .parent()
        .ok_or_else(|| failed("file must have a parent directory"))?;
    let mut file = NamedTempFile::new_in(parent).map_err(io_error)?;
    file.write_all(bytes).map_err(io_error)?;
    if let Some(permissions) = permissions {
        file.as_file()
            .set_permissions(permissions)
            .map_err(io_error)?;
    }
    file.as_file().sync_all().map_err(io_error)?;
    Ok(file)
}

pub(crate) fn ensure_active(context: &RunContext) -> Result<(), ToolError> {
    if context.cancellation.is_cancelled() {
        Err(failed("file operation cancelled"))
    } else {
        Ok(())
    }
}
fn failed(message: impl Into<String>) -> ToolError {
    ToolError::Failed(message.into())
}
fn io_error(error: std::io::Error) -> ToolError {
    failed(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FileSystemToolsConfig;

    #[test]
    fn failure_after_first_write_restores_old_content_and_reports_conflict() {
        let _guard = lock();
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("a"), b"old").unwrap();
        fs::write(root.path().join("b"), b"old").unwrap();
        let filesystem = FileSystemRoot::new(FileSystemToolsConfig::new(root.path()));
        let changes = ["a", "b"].map(|path| Change {
            path: path.into(),
            before: Some(b"old".to_vec()),
            after: Some(b"new".to_vec()),
            permissions: None,
        });
        let error = commit_with(&filesystem, &changes, None, |index| {
            if index == 1 {
                fs::write(root.path().join("b"), b"external writer").unwrap();
            }
        })
        .unwrap_err();
        assert!(error.to_string().contains("rolled back"));
        assert_eq!(fs::read(root.path().join("a")).unwrap(), b"old");
        assert_eq!(fs::read(root.path().join("b")).unwrap(), b"external writer");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
    }

    #[test]
    fn rollback_refuses_to_overwrite_a_later_external_edit() {
        let _guard = lock();
        let root = tempfile::tempdir().unwrap();
        for name in ["a", "b"] {
            fs::write(root.path().join(name), b"old").unwrap();
        }
        let filesystem = FileSystemRoot::new(FileSystemToolsConfig::new(root.path()));
        let changes = ["a", "b"].map(|path| Change {
            path: path.into(),
            before: Some(b"old".to_vec()),
            after: Some(b"new".to_vec()),
            permissions: None,
        });
        let error = commit_with(&filesystem, &changes, None, |index| {
            if index == 1 {
                for name in ["a", "b"] {
                    fs::write(root.path().join(name), b"external writer").unwrap();
                }
            }
        })
        .unwrap_err();
        assert!(matches!(error, ToolError::Indeterminate(_)));
        for name in ["a", "b"] {
            assert_eq!(
                fs::read(root.path().join(name)).unwrap(),
                b"external writer"
            );
        }
    }
}
