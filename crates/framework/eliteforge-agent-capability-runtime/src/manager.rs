use crate::*;
use base64::Engine;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

const MAX_PACKAGE_BYTES: usize = 128 * 1024 * 1024;
const MAX_UNPACKED_BYTES: u64 = 256 * 1024 * 1024;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallPolicy {
    Signed,
    Development,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledCapabilityPack {
    pub manifest: CapabilityPackManifest,
    pub digest: String,
    pub path: PathBuf,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackVersionRequirement {
    pub package: String,
    pub version: semver::Version,
    pub digest: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DurablePackPin {
    pub id: String,
    pub scope: String,
    pub versions: Vec<PackVersionRequirement>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ManagementEvent {
    Installed { package: String, version: String },
    Removed { package: String, version: String },
    ProcessExited { package: String, scope: String },
}
#[derive(Debug, Serialize, Deserialize)]
struct PackageSignature {
    key_id: String,
    signature: String,
}
#[derive(Debug, Clone)]
pub struct ExtensionCatalogEntry {
    pub contract: ExtensionContract,
    pub defining_package: String,
    pub implementations: Vec<(String, semver::Version, ExtensionImplementation)>,
}
#[derive(Clone)]
pub struct CapabilityPackManager {
    root: Arc<PathBuf>,
    keys: Arc<Mutex<BTreeMap<String, [u8; 32]>>>,
    pub(crate) usage: UsageRegistry,
    events: tokio::sync::broadcast::Sender<ManagementEvent>,
    pub(crate) instances: Arc<Mutex<BTreeMap<InstanceIdentity, std::sync::Weak<BackendInstance>>>>,
}
impl CapabilityPackManager {
    pub fn new(root: impl Into<PathBuf>) -> RuntimeResult<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        let root = std::fs::canonicalize(root)?;
        let keys_path = root.join("trusted-keys.json");
        let keys = if keys_path.exists() {
            serde_json::from_slice(&std::fs::read(keys_path)?)?
        } else {
            BTreeMap::new()
        };
        let (events, _) = tokio::sync::broadcast::channel(64);
        Ok(Self {
            root: Arc::new(root),
            keys: Arc::new(Mutex::new(keys)),
            usage: UsageRegistry::default(),
            events,
            instances: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }
    pub fn running_instances(&self) -> Vec<InstanceIdentity> {
        self.instances
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter_map(|(identity, instance)| {
                instance
                    .upgrade()
                    .filter(|instance| !instance.is_retired())
                    .map(|_| identity.clone())
            })
            .collect()
    }
    /// Actual choices in live scopes, separate from installed implementations.
    pub fn running_selections(&self) -> Vec<serde_json::Value> {
        self.instances.lock().unwrap_or_else(|e|e.into_inner()).values().filter_map(|instance|instance.upgrade()).filter(|instance|!instance.is_retired()).map(|instance|serde_json::json!({"instance":instance.identity,"implementations":instance.selected_implementations})).collect()
    }
    pub fn instance(&self, identity: &InstanceIdentity) -> RuntimeResult<Arc<BackendInstance>> {
        self.instances
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(identity)
            .and_then(std::sync::Weak::upgrade)
            .filter(|instance| !instance.is_retired())
            .ok_or_else(|| RuntimeError::Invalid("capability instance is unavailable".into()))
    }
    pub fn usage(&self) -> &UsageRegistry {
        &self.usage
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub(crate) fn version_lock(
        &self,
        package: &str,
        version: &semver::Version,
        exclusive: bool,
    ) -> RuntimeResult<std::fs::File> {
        validate_id(package)?;
        let directory = self.root.join("locks");
        std::fs::create_dir_all(&directory)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join(format!("{package}@{version}.lock")))?;
        let result = if exclusive {
            FileExt::try_lock_exclusive(&file)
        } else {
            FileExt::try_lock_shared(&file)
        };
        if let Err(error) = result {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Err(RuntimeError::Busy(vec![UsageBlocker {
                    package: package.into(),
                    version: version.to_string(),
                    scope: "shared version repository".into(),
                    kind: "version_lock".into(),
                    label: "version is referenced by another runtime or repository operation"
                        .into(),
                }]));
            }
            return Err(error.into());
        }
        Ok(file)
    }
    pub(crate) fn process_exited(&self, package: String, scope: String) {
        let _ = self
            .events
            .send(ManagementEvent::ProcessExited { package, scope });
    }
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<ManagementEvent> {
        self.events.subscribe()
    }
    fn lock_store(&self) -> RuntimeResult<std::fs::File> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join(".store.lock"))?;
        file.lock_exclusive()?;
        Ok(file)
    }
    pub fn trust_key(&self, id: &str, key: [u8; 32]) -> RuntimeResult<()> {
        validate_id(id)?;
        VerifyingKey::from_bytes(&key).map_err(|e| RuntimeError::Invalid(e.to_string()))?;
        let _lock = self.lock_store()?;
        let mut keys = self.keys.lock().unwrap_or_else(|e| e.into_inner());
        let path = self.root.join("trusted-keys.json");
        if path.exists() {
            *keys = serde_json::from_slice(&std::fs::read(&path)?)?;
        }
        keys.insert(id.to_owned(), key);
        atomic_json(&self.root.join("trusted-keys.json"), &*keys)
    }
    pub fn install_file(
        &self,
        file: impl AsRef<Path>,
        policy: InstallPolicy,
    ) -> RuntimeResult<InstalledCapabilityPack> {
        let metadata = std::fs::metadata(file.as_ref())?;
        if metadata.len() > MAX_PACKAGE_BYTES as u64 {
            return Err(RuntimeError::Invalid("package exceeds size limit".into()));
        }
        self.install_bytes(&std::fs::read(file)?, policy)
    }
    pub async fn install_url(
        &self,
        url: &str,
        policy: InstallPolicy,
    ) -> RuntimeResult<InstalledCapabilityPack> {
        let url = reqwest::Url::parse(url).map_err(|e| RuntimeError::Invalid(e.to_string()))?;
        if url.scheme() != "https" {
            return Err(RuntimeError::Invalid("package URL must use https".into()));
        }
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.url().scheme() != "https" || attempt.previous().len() >= 5 {
                    attempt.error("unsafe package redirect")
                } else {
                    attempt.follow()
                }
            }))
            .build()
            .map_err(|e| RuntimeError::Invalid(e.to_string()))?;
        let mut response = client
            .get(url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| RuntimeError::Invalid(e.to_string()))?;
        if response.url().scheme() != "https"
            || response
                .content_length()
                .is_some_and(|n| n > MAX_PACKAGE_BYTES as u64)
        {
            return Err(RuntimeError::Invalid(
                "unsafe redirect or oversized package".into(),
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| RuntimeError::Invalid(e.to_string()))?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_PACKAGE_BYTES {
                return Err(RuntimeError::Invalid("package exceeds size limit".into()));
            }
            bytes.extend_from_slice(&chunk);
        }
        let manager = self.clone();
        tokio::task::spawn_blocking(move || manager.install_bytes(&bytes, policy))
            .await
            .map_err(|e| RuntimeError::Invalid(e.to_string()))?
    }
    pub fn install_bytes(
        &self,
        bytes: &[u8],
        policy: InstallPolicy,
    ) -> RuntimeResult<InstalledCapabilityPack> {
        if bytes.len() > MAX_PACKAGE_BYTES {
            return Err(RuntimeError::Invalid("package exceeds size limit".into()));
        }
        let _lock = self.lock_store()?;
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
            .map_err(|e| RuntimeError::Invalid(e.to_string()))?;
        if archive.len() > 10000 {
            return Err(RuntimeError::Invalid("too many archive entries".into()));
        }
        let mut files = BTreeMap::new();
        let mut total = 0u64;
        for i in 0..archive.len() {
            let file = archive
                .by_index(i)
                .map_err(|e| RuntimeError::Invalid(e.to_string()))?;
            if file.is_dir() {
                continue;
            }
            safe_relative(file.name())?;
            if file.name() == "receipt.json" {
                return Err(RuntimeError::Invalid("receipt.json is reserved".into()));
            }
            if file.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000) {
                return Err(RuntimeError::Invalid(
                    "symlinks are not package resources".into(),
                ));
            }
            total = total.saturating_add(file.size());
            if total > MAX_UNPACKED_BYTES {
                return Err(RuntimeError::Invalid(
                    "unpacked package exceeds size limit".into(),
                ));
            }
            let name = file.name().to_owned();
            let mut data = Vec::new();
            file.take(MAX_UNPACKED_BYTES + 1).read_to_end(&mut data)?;
            if files.insert(name, data).is_some() {
                return Err(RuntimeError::Invalid("duplicate archive path".into()));
            }
        }
        let manifest: CapabilityPackManifest = serde_json::from_slice(
            files
                .get("manifest.json")
                .ok_or_else(|| RuntimeError::Invalid("missing manifest.json".into()))?,
        )?;
        manifest.validate()?;
        let checksums_data = files
            .get("checksums.json")
            .ok_or_else(|| RuntimeError::Invalid("missing checksums.json".into()))?;
        let checksums: BTreeMap<String, String> = serde_json::from_slice(checksums_data)?;
        let payloads: BTreeMap<_, _> = files
            .iter()
            .filter(|(name, _)| !matches!(name.as_str(), "checksums.json" | "signature.json"))
            .map(|(name, data)| (name.clone(), hash(data)))
            .collect();
        if checksums != payloads {
            return Err(RuntimeError::Invalid(
                "package checksums do not match every payload".into(),
            ));
        }
        if let Some(signature) = files.get("signature.json") {
            let signature: PackageSignature = serde_json::from_slice(signature)?;
            let keys = self.keys.lock().unwrap_or_else(|e| e.into_inner());
            let key = keys
                .get(&signature.key_id)
                .ok_or_else(|| RuntimeError::Invalid("untrusted signing key".into()))?;
            let key =
                VerifyingKey::from_bytes(key).map_err(|e| RuntimeError::Invalid(e.to_string()))?;
            let signature = base64::engine::general_purpose::STANDARD
                .decode(signature.signature)
                .map_err(|e| RuntimeError::Invalid(e.to_string()))?;
            let signature = Signature::from_slice(&signature)
                .map_err(|e| RuntimeError::Invalid(e.to_string()))?;
            key.verify(checksums_data, &signature)
                .map_err(|_| RuntimeError::Invalid("invalid package signature".into()))?;
        } else if policy == InstallPolicy::Signed {
            return Err(RuntimeError::Invalid(
                "unsigned package requires development policy".into(),
            ));
        }
        for path in manifest
            .pages
            .iter()
            .map(|p| &p.entry)
            .chain(manifest.backend.iter().flat_map(|b| b.executables.values()))
            .chain(manifest.mcp.iter().filter_map(|m| {
                if let McpDeclarationTransport::Stdio { program, .. } = &m.transport {
                    Some(program)
                } else {
                    None
                }
            }))
            .chain(&manifest.resources)
        {
            if !files.contains_key(path) {
                return Err(RuntimeError::Invalid(format!(
                    "missing declared resource: {path}"
                )));
            }
        }
        for directory in &manifest.skills {
            let prefix = format!("{directory}/");
            if !files.keys().any(|name| name.starts_with(&prefix)) {
                return Err(RuntimeError::Invalid(format!(
                    "missing declared skills directory: {directory}"
                )));
            }
        }
        let digest = hash(&serde_json::to_vec(&checksums)?);
        let versions = self.root.join("packs").join(&manifest.id).join("versions");
        std::fs::create_dir_all(&versions)?;
        let destination = versions.join(manifest.version.to_string());
        if destination.exists() {
            let installed = self.read_pack(&destination)?;
            if installed.digest == digest {
                return Ok(installed);
            }
            return Err(RuntimeError::Invalid(
                "an installed version is immutable; increment its version".into(),
            ));
        }
        let stage = versions.join(format!(".stage-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&stage)?;
        let stage_guard = StageGuard(stage.clone());
        for (name, data) in &files {
            let path = stage.join(safe_relative(name)?);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, data)?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            {
                for executable in manifest
                    .backend
                    .iter()
                    .flat_map(|b| b.executables.values())
                    .chain(manifest.mcp.iter().filter_map(|m| {
                        if let McpDeclarationTransport::Stdio { program, .. } = &m.transport {
                            Some(program)
                        } else {
                            None
                        }
                    }))
                {
                    std::fs::set_permissions(
                        stage.join(executable),
                        std::fs::Permissions::from_mode(0o700),
                    )?;
                }
            }
        }
        atomic_json(&stage.join("receipt.json"), &digest)?;
        std::fs::rename(&stage, &destination)?;
        drop(stage_guard);
        let _ = self.events.send(ManagementEvent::Installed {
            package: manifest.id.clone(),
            version: manifest.version.to_string(),
        });
        Ok(InstalledCapabilityPack {
            manifest,
            digest,
            path: destination,
        })
    }
    pub(crate) fn read_pack(&self, path: &Path) -> RuntimeResult<InstalledCapabilityPack> {
        let manifest: CapabilityPackManifest =
            serde_json::from_slice(&std::fs::read(path.join("manifest.json"))?)?;
        manifest.validate()?;
        let digest: String = serde_json::from_slice(&std::fs::read(path.join("receipt.json"))?)?;
        let checksums: BTreeMap<String, String> =
            serde_json::from_slice(&std::fs::read(path.join("checksums.json"))?)?;
        if path.join("signature.json").exists() {
            let signature: PackageSignature =
                serde_json::from_slice(&std::fs::read(path.join("signature.json"))?)?;
            let keys = self.keys.lock().unwrap_or_else(|e| e.into_inner());
            let key = keys
                .get(&signature.key_id)
                .ok_or_else(|| RuntimeError::Invalid("untrusted installed signing key".into()))?;
            let signature = base64::engine::general_purpose::STANDARD
                .decode(signature.signature)
                .map_err(|e| RuntimeError::Invalid(e.to_string()))?;
            VerifyingKey::from_bytes(key)
                .map_err(|e| RuntimeError::Invalid(e.to_string()))?
                .verify(
                    &std::fs::read(path.join("checksums.json"))?,
                    &Signature::from_slice(&signature)
                        .map_err(|e| RuntimeError::Invalid(e.to_string()))?,
                )
                .map_err(|_| RuntimeError::Invalid("installed package signature changed".into()))?;
        }
        if digest != hash(&serde_json::to_vec(&checksums)?) {
            return Err(RuntimeError::Invalid("corrupt installation receipt".into()));
        }
        let canonical = std::fs::canonicalize(path)?;
        for (relative, expected) in checksums {
            let file = std::fs::canonicalize(path.join(safe_relative(&relative)?))?;
            if !file.starts_with(&canonical) || hash(&std::fs::read(file)?) != expected {
                return Err(RuntimeError::Invalid(format!(
                    "installed payload changed: {relative}"
                )));
            }
        }
        Ok(InstalledCapabilityPack {
            manifest,
            digest,
            path: path.to_path_buf(),
        })
    }
    pub fn installed(&self) -> RuntimeResult<Vec<InstalledCapabilityPack>> {
        let mut result = Vec::new();
        let root = self.root.join("packs");
        if !root.exists() {
            return Ok(result);
        }
        for package in std::fs::read_dir(root)? {
            let versions = package?.path().join("versions");
            if !versions.exists() {
                continue;
            }
            for version in std::fs::read_dir(versions)? {
                let path = version?.path();
                if path
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with('.'))
                {
                    continue;
                }
                result.push(self.read_pack(&path)?);
            }
        }
        result.sort_by(|a, b| {
            (&a.manifest.id, &a.manifest.version).cmp(&(&b.manifest.id, &b.manifest.version))
        });
        Ok(result)
    }
    pub fn resolve(
        &self,
        package: &str,
        requirement: &semver::VersionReq,
    ) -> RuntimeResult<Vec<InstalledCapabilityPack>> {
        let available = self.installed()?;
        // Recompute constraints at each branch so a later sibling can force a
        // previously selected dependency to a lower compatible version.
        fn search(
            root: &str,
            req: &semver::VersionReq,
            available: &[InstalledCapabilityPack],
            selected: BTreeMap<String, InstalledCapabilityPack>,
            attempts: &mut usize,
        ) -> Option<BTreeMap<String, InstalledCapabilityPack>> {
            *attempts += 1;
            if *attempts > 10000 {
                return None;
            }
            let mut constraints = BTreeMap::<String, Vec<semver::VersionReq>>::from([(
                root.into(),
                vec![req.clone()],
            )]);
            for pack in selected.values() {
                for dependency in &pack.manifest.dependencies {
                    constraints
                        .entry(dependency.id.clone())
                        .or_default()
                        .push(dependency.version.clone());
                }
            }
            if selected.iter().any(|(id, pack)| {
                constraints.get(id).is_some_and(|requirements| {
                    requirements
                        .iter()
                        .any(|r| !r.matches(&pack.manifest.version))
                })
            }) {
                return None;
            }
            let next = constraints
                .iter()
                .find(|(id, _)| !selected.contains_key(*id));
            let Some((id, requirements)) = next else {
                fn acyclic(
                    id: &str,
                    selected: &BTreeMap<String, InstalledCapabilityPack>,
                    active: &mut HashSet<String>,
                    done: &mut HashSet<String>,
                ) -> bool {
                    if done.contains(id) {
                        return true;
                    }
                    if !active.insert(id.into()) {
                        return false;
                    }
                    for dependency in &selected[id].manifest.dependencies {
                        if !acyclic(&dependency.id, selected, active, done) {
                            return false;
                        }
                    }
                    active.remove(id);
                    done.insert(id.into());
                    true
                }
                return acyclic(root, &selected, &mut HashSet::new(), &mut HashSet::new())
                    .then_some(selected);
            };
            let mut candidates: Vec<_> = available
                .iter()
                .filter(|p| {
                    p.manifest.id == *id
                        && requirements.iter().all(|r| r.matches(&p.manifest.version))
                })
                .collect();
            candidates.sort_by(|a, b| b.manifest.version.cmp(&a.manifest.version));
            for pack in candidates {
                let mut branch = selected.clone();
                branch.insert(id.clone(), pack.clone());
                if let Some(solution) = search(root, req, available, branch, attempts) {
                    return Some(solution);
                }
            }
            None
        }
        let selected = search(package, requirement, &available, BTreeMap::new(), &mut 0)
            .ok_or_else(|| {
                RuntimeError::Invalid(format!(
                    "no compatible acyclic dependency graph for {package} (maximum 10000 branches)"
                ))
            })?;
        let mut contracts: BTreeMap<_, _> = framework_extension_contracts()
            .into_iter()
            .map(|c| (c.id.clone(), c))
            .collect();
        let builtin_contracts: std::collections::BTreeSet<_> = contracts.keys().cloned().collect();
        for pack in selected.values() {
            for contract in &pack.manifest.extension_points {
                if contracts
                    .insert(contract.id.clone(), contract.clone())
                    .is_some()
                {
                    return Err(RuntimeError::Invalid(format!(
                        "duplicate contract in selected graph: {}",
                        contract.id
                    )));
                }
            }
        }
        for pack in selected.values() {
            for implementation in &pack.manifest.implementations {
                let contract = contracts.get(&implementation.extension).ok_or_else(|| {
                    RuntimeError::Invalid(format!("missing contract: {}", implementation.extension))
                })?;
                if !implementation.contract.matches(&contract.version) {
                    return Err(RuntimeError::Invalid(format!(
                        "incompatible contract: {}",
                        implementation.extension
                    )));
                }
                let defining_package = if builtin_contracts.contains(&contract.id) {
                    "framework"
                } else {
                    selected
                        .values()
                        .find(|p| {
                            p.manifest
                                .extension_points
                                .iter()
                                .any(|c| c.id == contract.id)
                        })
                        .map(|p| p.manifest.id.as_str())
                        .ok_or_else(|| {
                            RuntimeError::Invalid("contract definition package missing".into())
                        })?
                };
                if defining_package != "framework"
                    && defining_package != pack.manifest.id
                    && !pack
                        .manifest
                        .dependencies
                        .iter()
                        .any(|d| d.id == defining_package)
                {
                    return Err(RuntimeError::Invalid(format!(
                        "{} must declare dependency on contract owner {}",
                        pack.manifest.id, defining_package
                    )));
                }
            }
        }
        Ok(selected.into_values().collect())
    }
    /// Every installed contract version remains visible, including older versions.
    pub fn extension_catalog(&self) -> RuntimeResult<Vec<ExtensionCatalogEntry>> {
        let installed = self.installed()?;
        let mut contracts: BTreeMap<_, _> = framework_extension_contracts()
            .into_iter()
            .map(|c| {
                (
                    (c.id.clone(), c.version.clone()),
                    ExtensionCatalogEntry {
                        defining_package: "framework".into(),
                        contract: c,
                        implementations: Vec::new(),
                    },
                )
            })
            .collect();
        for pack in &installed {
            for contract in &pack.manifest.extension_points {
                let key = (contract.id.clone(), contract.version.clone());
                if let Some(existing) = contracts.get(&key) {
                    if existing.defining_package != pack.manifest.id
                        || serde_json::to_value(&existing.contract)?
                            != serde_json::to_value(contract)?
                    {
                        return Err(RuntimeError::Invalid(format!(
                            "conflicting contract: {}@{}",
                            contract.id, contract.version
                        )));
                    }
                } else {
                    contracts.insert(
                        key,
                        ExtensionCatalogEntry {
                            defining_package: pack.manifest.id.clone(),
                            contract: contract.clone(),
                            implementations: Vec::new(),
                        },
                    );
                }
            }
        }
        for pack in installed {
            for implementation in pack.manifest.implementations {
                let mut matched = false;
                for ((id, version), entry) in &mut contracts {
                    if id == &implementation.extension && implementation.contract.matches(version) {
                        entry.implementations.push((
                            pack.manifest.id.clone(),
                            pack.manifest.version.clone(),
                            implementation.clone(),
                        ));
                        matched = true;
                    }
                }
                if !matched {
                    return Err(RuntimeError::Invalid(format!(
                        "missing compatible contract: {}",
                        implementation.extension
                    )));
                }
            }
        }
        Ok(contracts.into_values().collect())
    }
    pub fn durable_pins(&self) -> RuntimeResult<Vec<DurablePackPin>> {
        let path = self.root.join("pins.json");
        if path.exists() {
            Ok(serde_json::from_slice(&std::fs::read(path)?)?)
        } else {
            Ok(Vec::new())
        }
    }
    pub fn pin(&self, pin: DurablePackPin) -> RuntimeResult<()> {
        let _lock = self.lock_store()?;
        let mut pins = self.durable_pins()?;
        self.validate_versions(&pin.versions)?;
        if let Some(existing) = pins.iter().find(|existing| existing.id == pin.id) {
            if serde_json::to_value(existing)? != serde_json::to_value(&pin)? {
                return Err(RuntimeError::Invalid(
                    "a pending task cannot change its capability version requirement".into(),
                ));
            }
            return Ok(());
        }
        pins.retain(|p| p.id != pin.id);
        pins.push(pin);
        atomic_json(&self.root.join("pins.json"), &pins)
    }
    pub fn unpin(&self, id: &str) -> RuntimeResult<()> {
        let _lock = self.lock_store()?;
        let mut pins = self.durable_pins()?;
        pins.retain(|p| p.id != id);
        atomic_json(&self.root.join("pins.json"), &pins)
    }
    pub fn validate_versions(&self, versions: &[PackVersionRequirement]) -> RuntimeResult<()> {
        let installed = self.installed()?;
        for version in versions {
            if !installed.iter().any(|p| {
                p.manifest.id == version.package
                    && p.manifest.version == version.version
                    && p.digest == version.digest
            }) {
                return Err(RuntimeError::Invalid(format!(
                    "required capability version unavailable: {}@{}",
                    version.package, version.version
                )));
            }
        }
        Ok(())
    }
    pub fn remove(&self, package: &str, version: &semver::Version) -> RuntimeResult<()> {
        validate_id(package)?;
        let _lock = self.lock_store()?;
        let _version_lock = self.version_lock(package, version, true)?;
        let _update = self
            .usage
            .begin_version_removal(package, &version.to_string())?;
        let blockers: Vec<_> = self
            .durable_pins()?
            .into_iter()
            .flat_map(|pin| {
                pin.versions
                    .into_iter()
                    .filter(move |v| v.package == package && &v.version == version)
                    .map(move |v| UsageBlocker {
                        package: v.package,
                        version: v.version.to_string(),
                        scope: pin.scope.clone(),
                        kind: "durable_pin".into(),
                        label: pin.id.clone(),
                    })
            })
            .collect();
        if !blockers.is_empty() {
            return Err(RuntimeError::Busy(blockers));
        }
        std::fs::remove_dir_all(
            self.root
                .join("packs")
                .join(package)
                .join("versions")
                .join(version.to_string()),
        )?;
        let _ = self.events.send(ManagementEvent::Removed {
            package: package.into(),
            version: version.to_string(),
        });
        Ok(())
    }
    pub fn read_resource(
        &self,
        pack: &InstalledCapabilityPack,
        relative: &str,
    ) -> RuntimeResult<Vec<u8>> {
        let expected = self
            .root
            .join("packs")
            .join(&pack.manifest.id)
            .join("versions")
            .join(pack.manifest.version.to_string());
        if pack.path != expected {
            return Err(RuntimeError::Invalid(
                "resource is not in this version repository".into(),
            ));
        }
        let verified = self.read_pack(&expected)?;
        if verified.digest != pack.digest {
            return Err(RuntimeError::Invalid(
                "resource version identity mismatch".into(),
            ));
        }
        let path = std::fs::canonicalize(pack.path.join(safe_relative(relative)?))?;
        if !path.starts_with(std::fs::canonicalize(&pack.path)?) {
            return Err(RuntimeError::Invalid("resource escapes package".into()));
        }
        Ok(std::fs::read(path)?)
    }
}
pub(crate) fn hash(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}
fn atomic_json(path: &Path, data: &impl Serialize) -> RuntimeResult<()> {
    let temporary = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    let guard = StageGuard(temporary.clone());
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(&serde_json::to_vec(data)?)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(temporary, path)?;
    drop(guard);
    Ok(())
}
struct StageGuard(PathBuf);
impl Drop for StageGuard {
    fn drop(&mut self) {
        if self.0.is_dir() {
            let _ = std::fs::remove_dir_all(&self.0);
        } else {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}
