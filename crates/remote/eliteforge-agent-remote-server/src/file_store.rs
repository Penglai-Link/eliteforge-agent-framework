//! Small file-backed store for products that do not already have settings storage.

use crate::RemoteStore;
use async_trait::async_trait;
use eliteforge_agent_remote_protocol::{DeviceRecord, NodeIdentity, RemoteSettings};
use std::path::{Path, PathBuf};
use tokio::fs;

#[derive(Clone)]
pub struct FileRemoteStore {
    root: PathBuf,
}

impl FileRemoteStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn settings_path(&self) -> PathBuf {
        self.root.join("settings.json")
    }

    fn identity_path(&self) -> PathBuf {
        self.root.join("identity.json")
    }

    fn devices_path(&self) -> PathBuf {
        self.root.join("devices.json")
    }

    async fn ensure_root(&self) -> Result<(), String> {
        fs::create_dir_all(&self.root)
            .await
            .map_err(|e| format!("create remote-access dir: {e}"))
    }

    async fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, String> {
        match fs::read(path).await {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| format!("parse {}: {e}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("read {}: {error}", path.display())),
        }
    }

    async fn write_json<T: serde::Serialize>(&self, path: &Path, value: &T) -> Result<(), String> {
        self.ensure_root().await?;
        let bytes = serde_json::to_vec_pretty(value).map_err(|e| format!("serialize: {e}"))?;
        let temp = path.with_extension("tmp");
        fs::write(&temp, &bytes)
            .await
            .map_err(|e| format!("write {}: {e}", temp.display()))?;
        set_private_permissions(&temp).await?;
        fs::rename(&temp, path)
            .await
            .map_err(|e| format!("rename {}: {e}", path.display()))
    }
}

#[async_trait]
impl RemoteStore for FileRemoteStore {
    async fn load_settings(&self) -> Result<RemoteSettings, String> {
        Ok(Self::read_json(&self.settings_path())
            .await?
            .unwrap_or_default())
    }

    async fn save_settings(&self, settings: &RemoteSettings) -> Result<(), String> {
        self.write_json(&self.settings_path(), settings).await
    }

    async fn load_node_identity(&self) -> Result<Option<NodeIdentity>, String> {
        Self::read_json(&self.identity_path()).await
    }

    async fn save_node_identity(&self, identity: &NodeIdentity) -> Result<(), String> {
        self.write_json(&self.identity_path(), identity).await
    }

    async fn load_devices(&self) -> Result<Vec<DeviceRecord>, String> {
        Ok(Self::read_json(&self.devices_path())
            .await?
            .unwrap_or_default())
    }

    async fn save_devices(&self, devices: &[DeviceRecord]) -> Result<(), String> {
        self.write_json(&self.devices_path(), &devices).await
    }
}

async fn set_private_permissions(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = std::fs::Permissions::from_mode(0o600);
        fs::set_permissions(path, permissions)
            .await
            .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    }
    Ok(())
}
