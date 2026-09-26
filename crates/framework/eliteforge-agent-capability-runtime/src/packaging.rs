use crate::*;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use std::{collections::BTreeMap, io::Write, path::Path};

/// Build the same checksummed archive accepted by the production installer.
/// An absent signing key creates a development-only archive.
pub fn build_capability_archive(
    directory: &Path,
    signing: Option<(&str, &SigningKey)>,
) -> RuntimeResult<Vec<u8>> {
    fn collect(
        root: &Path,
        path: &Path,
        files: &mut BTreeMap<String, Vec<u8>>,
        total: &mut usize,
    ) -> RuntimeResult<()> {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let metadata = entry.file_type()?;
            if metadata.is_symlink() {
                return Err(RuntimeError::Invalid(
                    "package cannot contain symlinks".into(),
                ));
            }
            if metadata.is_dir() {
                collect(root, &entry.path(), files, total)?;
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(root)
                .map_err(|e| RuntimeError::Invalid(e.to_string()))?
                .to_string_lossy()
                .replace('\\', "/");
            safe_relative(&relative)?;
            if matches!(
                relative.as_str(),
                "checksums.json" | "signature.json" | "receipt.json"
            ) {
                return Err(RuntimeError::Invalid(format!(
                    "reserved package path: {relative}"
                )));
            }
            let size = entry.metadata()?.len();
            if size > 128 * 1024 * 1024
                || (*total as u64).saturating_add(size) > 256 * 1024 * 1024
                || files.len() >= 10000
            {
                return Err(RuntimeError::Invalid("package exceeds size limit".into()));
            }
            let bytes = std::fs::read(entry.path())?;
            *total += bytes.len();
            files.insert(relative, bytes);
        }
        Ok(())
    }
    let root = std::fs::canonicalize(directory)?;
    let mut files = BTreeMap::new();
    collect(&root, &root, &mut files, &mut 0)?;
    let manifest: CapabilityPackManifest = serde_json::from_slice(
        files
            .get("manifest.json")
            .ok_or_else(|| RuntimeError::Invalid("missing manifest.json".into()))?,
    )?;
    manifest.validate()?;
    let checksums: BTreeMap<_, _> = files
        .iter()
        .map(|(name, bytes)| (name.clone(), crate::manager::hash(bytes)))
        .collect();
    let checksum_bytes = serde_json::to_vec(&checksums)?;
    if let Some((id, key)) = signing {
        validate_id(id)?;
        files.insert("signature.json".into(),serde_json::to_vec(&serde_json::json!({"key_id":id,"signature":base64::engine::general_purpose::STANDARD.encode(key.sign(&checksum_bytes).to_bytes())}))?);
    }
    files.insert("checksums.json".into(), checksum_bytes);
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, bytes) in files {
        writer
            .start_file(
                name,
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated),
            )
            .map_err(|e| RuntimeError::Invalid(e.to_string()))?;
        writer.write_all(&bytes)?;
    }
    let archive = writer
        .finish()
        .map_err(|e| RuntimeError::Invalid(e.to_string()))?
        .into_inner();
    if archive.len() > 128 * 1024 * 1024 {
        return Err(RuntimeError::Invalid("archive exceeds size limit".into()));
    }
    Ok(archive)
}
