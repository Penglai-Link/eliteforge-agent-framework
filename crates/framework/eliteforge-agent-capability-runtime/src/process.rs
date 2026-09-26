use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{
    process::Command,
    sync::{Mutex, Notify, mpsc},
};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct InstanceIdentity {
    pub package: String,
    pub version: semver::Version,
    pub digest: String,
    pub scope: String,
    pub configuration_version: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendHandshake {
    pub protocol: u32,
    pub package: String,
    pub version: semver::Version,
    pub implementations: Vec<String>,
}
struct ProcessSession {
    peer: RpcPeer,
    stop: mpsc::Sender<()>,
    finished: Arc<Notify>,
    exited: Arc<std::sync::atomic::AtomicBool>,
}
type BoundVideoJob = (
    String,
    eliteforge_agent_core::VideoJob,
    Option<UsageLease>,
    tokio::sync::OwnedSemaphorePermit,
);
/// One package instance, shared by all selected implementations in one scope.
/// Creating this value is inert: static packages never start a process.
pub struct BackendInstance {
    pub identity: InstanceIdentity,
    pack: InstalledCapabilityPack,
    manager: CapabilityPackManager,
    configuration: Value,
    session: Mutex<Option<ProcessSession>>,
    selection: std::sync::Mutex<Option<UsageLease>>,
    version_lock: Arc<std::sync::Mutex<Option<std::fs::File>>>,
    retired: Arc<std::sync::atomic::AtomicBool>,
    admission_paused: std::sync::atomic::AtomicBool,
    pub(crate) data_scope: std::sync::Mutex<String>,
    pub selected_implementations: std::collections::BTreeSet<String>,
    selected_pages: std::sync::Mutex<Option<std::collections::BTreeSet<String>>>,
    selected_plugins: std::sync::Mutex<Option<std::collections::BTreeSet<String>>>,
    state_leases: std::sync::Mutex<std::collections::BTreeMap<String, UsageLease>>,
    video_jobs: std::sync::Mutex<std::collections::BTreeMap<String, BoundVideoJob>>,
    video_capacity: Arc<tokio::sync::Semaphore>,
}
impl BackendInstance {
    pub fn new(
        manager: CapabilityPackManager,
        pack: InstalledCapabilityPack,
        scope: impl Into<String>,
        configuration_version: impl Into<String>,
        configuration: Value,
    ) -> RuntimeResult<Arc<Self>> {
        let selected = pack
            .manifest
            .implementations
            .iter()
            .map(|i| i.id.clone())
            .collect();
        Self::new_selected(
            manager,
            pack,
            scope,
            configuration_version,
            configuration,
            selected,
        )
    }
    pub fn new_selected(
        manager: CapabilityPackManager,
        pack: InstalledCapabilityPack,
        scope: impl Into<String>,
        configuration_version: impl Into<String>,
        configuration: Value,
        selected: std::collections::BTreeSet<String>,
    ) -> RuntimeResult<Arc<Self>> {
        let scope = scope.into();
        let configuration_version = configuration_version.into();
        if scope.is_empty() || configuration_version.is_empty() {
            return Err(RuntimeError::Invalid(
                "instance scope and configuration version are required".into(),
            ));
        }
        let version_lock =
            manager.version_lock(&pack.manifest.id, &pack.manifest.version, false)?;
        manager.validate_versions(&[PackVersionRequirement {
            package: pack.manifest.id.clone(),
            version: pack.manifest.version.clone(),
            digest: pack.digest.clone(),
        }])?;
        let identity = InstanceIdentity {
            package: pack.manifest.id.clone(),
            version: pack.manifest.version.clone(),
            digest: pack.digest.clone(),
            scope,
            configuration_version,
        };
        let registry = manager.instances.clone();
        let mut instances = registry.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = instances
            .get(&identity)
            .and_then(std::sync::Weak::upgrade)
            .filter(|instance| !instance.is_retired())
        {
            if existing.configuration != configuration
                || existing.selected_implementations != selected
            {
                return Err(RuntimeError::Invalid(
                    "same instance identity has different configuration or selection".into(),
                ));
            }
            return Ok(existing);
        }
        if selected
            .iter()
            .any(|id| !pack.manifest.implementations.iter().any(|i| &i.id == id))
        {
            return Err(RuntimeError::Invalid(
                "instance selects an unknown implementation".into(),
            ));
        }
        let selection = manager.usage.acquire(UsageBlocker {
            package: identity.package.clone(),
            version: identity.version.to_string(),
            scope: identity.scope.clone(),
            kind: "selection".into(),
            label: "selected capability package".into(),
        })?;
        let instance = Arc::new(Self {
            identity: identity.clone(),
            data_scope: std::sync::Mutex::new(identity.scope.clone()),
            pack,
            manager,
            configuration,
            session: Mutex::new(None),
            selection: std::sync::Mutex::new(Some(selection)),
            version_lock: Arc::new(std::sync::Mutex::new(Some(version_lock))),
            retired: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            admission_paused: std::sync::atomic::AtomicBool::new(false),
            selected_implementations: selected,
            selected_pages: std::sync::Mutex::new(None),
            selected_plugins: std::sync::Mutex::new(None),
            state_leases: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            video_jobs: std::sync::Mutex::new(Default::default()),
            video_capacity: Arc::new(tokio::sync::Semaphore::new(256)),
        });
        instances.insert(identity, Arc::downgrade(&instance));
        Ok(instance)
    }
    pub fn is_retired(&self) -> bool {
        self.retired.load(std::sync::atomic::Ordering::Acquire)
    }
    pub(crate) fn pause_admission(&self) {
        self.admission_paused
            .store(true, std::sync::atomic::Ordering::Release);
    }
    pub(crate) fn resume_admission(&self) {
        self.admission_paused
            .store(false, std::sync::atomic::Ordering::Release);
    }
    pub(crate) fn select_pages(
        &self,
        pages: std::collections::BTreeSet<String>,
    ) -> RuntimeResult<()> {
        let mut current = self
            .selected_pages
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if current.as_ref().is_some_and(|current| current != &pages) {
            return Err(RuntimeError::Invalid(
                "instance page selection differs".into(),
            ));
        }
        *current = Some(pages);
        Ok(())
    }
    pub(crate) fn select_plugins(
        &self,
        plugins: std::collections::BTreeSet<String>,
    ) -> RuntimeResult<()> {
        let mut current = self
            .selected_plugins
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if current.as_ref().is_some_and(|current| current != &plugins) {
            return Err(RuntimeError::Invalid(
                "instance plugin selection differs".into(),
            ));
        }
        *current = Some(plugins);
        Ok(())
    }
    fn contributions(&self) -> BackendContributions {
        BackendContributions {
            implementations: self.selected_implementations.iter().cloned().collect(),
            plugins: self
                .selected_plugins
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .unwrap_or_else(|| self.pack.manifest.plugins.iter().cloned().collect())
                .into_iter()
                .collect(),
            pages: self
                .selected_pages
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .unwrap_or_else(|| {
                    self.pack
                        .manifest
                        .pages
                        .iter()
                        .map(|p| p.id.clone())
                        .collect()
                })
                .into_iter()
                .collect(),
        }
    }
    pub fn page_is_selected(&self, page: &str) -> bool {
        self.selected_pages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_none_or(|pages| pages.contains(page))
    }
    fn state_lease_key(
        implementation: &str,
        lease: &eliteforge_agent_core::ExecutionLease,
    ) -> String {
        format!(
            "{implementation}:{}:{}:{}",
            lease.run_id, lease.owner, lease.generation
        )
    }
    pub(crate) fn track_state_lease(
        &self,
        implementation: &str,
        lease: &eliteforge_agent_core::ExecutionLease,
    ) -> RuntimeResult<()> {
        let occupancy = self.occupy("execution_lease", lease.run_id.to_string())?;
        self.state_leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(Self::state_lease_key(implementation, lease), occupancy);
        Ok(())
    }
    pub(crate) fn owns_state_lease(
        &self,
        implementation: &str,
        lease: &eliteforge_agent_core::ExecutionLease,
    ) -> bool {
        !self.is_retired()
            && self
                .state_leases
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&Self::state_lease_key(implementation, lease))
    }
    pub(crate) fn forget_state_lease(
        &self,
        implementation: &str,
        lease: &eliteforge_agent_core::ExecutionLease,
    ) {
        self.state_leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&Self::state_lease_key(implementation, lease));
    }
    pub(crate) fn reserve_video_job(&self) -> RuntimeResult<tokio::sync::OwnedSemaphorePermit> {
        self.video_capacity
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                RuntimeError::Invalid(
                    "video job handle capacity reached; cancel unused handles".into(),
                )
            })
    }
    pub(crate) fn bind_video_job(
        &self,
        implementation: &str,
        mut job: eliteforge_agent_core::VideoJob,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> RuntimeResult<eliteforge_agent_core::VideoJob> {
        let handle = uuid::Uuid::new_v4().to_string();
        let lease = if matches!(
            job.status,
            eliteforge_agent_core::ModelJobStatus::Queued
                | eliteforge_agent_core::ModelJobStatus::Running
        ) {
            Some(self.occupy("model_job", &handle)?)
        } else {
            None
        };
        self.video_jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                handle.clone(),
                (implementation.into(), job.clone(), lease, permit),
            );
        job.id = handle;
        Ok(job)
    }
    pub(crate) fn video_job(
        &self,
        implementation: &str,
        handle: &str,
    ) -> RuntimeResult<eliteforge_agent_core::VideoJob> {
        let jobs = self.video_jobs.lock().unwrap_or_else(|e| e.into_inner());
        let (owner, job, _, _) = jobs.get(handle).ok_or_else(|| {
            RuntimeError::Invalid("video job belongs to a closed or different instance".into())
        })?;
        if owner != implementation || self.is_retired() {
            return Err(RuntimeError::Invalid("video job owner mismatch".into()));
        }
        Ok(job.clone())
    }
    pub(crate) fn update_video_job(
        &self,
        handle: &str,
        mut job: eliteforge_agent_core::VideoJob,
    ) -> RuntimeResult<eliteforge_agent_core::VideoJob> {
        let mut jobs = self.video_jobs.lock().unwrap_or_else(|e| e.into_inner());
        let (_, current, lease, _) = jobs
            .get_mut(handle)
            .ok_or_else(|| RuntimeError::Invalid("video job handle closed".into()))?;
        *current = job.clone();
        if !matches!(
            job.status,
            eliteforge_agent_core::ModelJobStatus::Queued
                | eliteforge_agent_core::ModelJobStatus::Running
        ) {
            lease.take();
        }
        job.id = handle.into();
        Ok(job)
    }
    pub(crate) fn forget_video_job(&self, handle: &str) {
        self.video_jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(handle);
    }
    fn admits_operations(&self) -> bool {
        !self.is_retired()
            && !self
                .admission_paused
                .load(std::sync::atomic::Ordering::Acquire)
    }
    pub fn data_directory(&self) -> RuntimeResult<std::path::PathBuf> {
        let data = self
            .manager
            .root()
            .join("data")
            .join(&self.identity.package)
            .join(crate::manager::hash(
                self.data_scope
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_bytes(),
            ));
        std::fs::create_dir_all(&data)?;
        Ok(data)
    }
    pub fn read_resource(&self, relative: &str) -> RuntimeResult<Vec<u8>> {
        self.manager.read_resource(&self.pack, relative)
    }
    pub fn pack(&self) -> &InstalledCapabilityPack {
        &self.pack
    }
    pub fn occupy(&self, kind: &str, label: impl Into<String>) -> RuntimeResult<UsageLease> {
        if !self.admits_operations() {
            return Err(RuntimeError::Invalid(
                "instance is retired or reconfiguring".into(),
            ));
        }
        let lease = self.manager.usage.acquire(UsageBlocker {
            package: self.identity.package.clone(),
            version: self.identity.version.to_string(),
            scope: self.identity.scope.clone(),
            kind: kind.into(),
            label: label.into(),
        })?;
        if !self.admits_operations() {
            return Err(RuntimeError::Invalid(
                "instance is retired or reconfiguring".into(),
            ));
        }
        Ok(lease)
    }
    pub async fn peer(&self) -> RuntimeResult<RpcPeer> {
        if !self.admits_operations() {
            return Err(RuntimeError::Rpc(
                "backend instance is retired or reconfiguring".into(),
            ));
        }
        let mut session = self.session.lock().await;
        if let Some(session) = &*session {
            if session.peer.is_closed() {
                return Err(RuntimeError::Rpc(
                    "backend exited; explicit rebuild is required".into(),
                ));
            }
            return Ok(session.peer.clone());
        }
        let pack = self.manager.read_pack(&self.pack.path)?;
        let backend = pack.manifest.backend.as_ref().ok_or_else(|| {
            RuntimeError::Invalid("this capability package has no backend".into())
        })?;
        let platform = platform_key();
        let executable = backend.executables.get(&platform).ok_or_else(|| {
            RuntimeError::Invalid(format!("backend is unavailable for {platform}"))
        })?;
        let executable = std::fs::canonicalize(pack.path.join(safe_relative(executable)?))?;
        if !executable.starts_with(std::fs::canonicalize(&pack.path)?) {
            return Err(RuntimeError::Invalid(
                "backend escapes installed package".into(),
            ));
        }
        let data = self.data_directory()?;
        let contributions = self.contributions();
        let mut command = Command::new(executable);
        command
            .args(&backend.args)
            .current_dir(data)
            .env("CAPABILITY_PACK_DIR", &pack.path)
            .env(
                "CAPABILITY_INSTANCE",
                serde_json::to_string(&self.identity)?,
            )
            .env(
                "CAPABILITY_SELECTION",
                serde_json::to_string(&self.selected_implementations)?,
            )
            .env(
                "CAPABILITY_CONTRIBUTIONS",
                serde_json::to_string(&contributions)?,
            )
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            command.process_group(0);
        }
        let mut child = command.spawn()?;
        let pid = child.id();
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| RuntimeError::Invalid("backend stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| RuntimeError::Invalid("backend stdout unavailable".into()))?;
        let peer = RpcPeer::new(stdout, stdin, "host", Arc::new(RejectRequests));
        let (stop, mut stopping) = mpsc::channel(1);
        let finished = Arc::new(Notify::new());
        let exited = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let completion = finished.clone();
        let exit_flag = exited.clone();
        let monitor_peer = peer.clone();
        let runtime_lease = self.occupy("runtime", "backend process")?;
        let manager = self.manager.clone();
        let identity = self.identity.clone();
        let retired = self.retired.clone();
        let version_lock = self.version_lock.clone();
        tokio::spawn(async move {
            tokio::select! { _ = child.wait() => {}, _ = stopping.recv() => {}, _ = monitor_peer.closed() => {} }
            retired.store(true, std::sync::atomic::Ordering::Release);
            monitor_peer.close();
            terminate_tree(pid).await;
            let _ = child.kill().await;
            let _ = child.wait().await;
            drop(runtime_lease);
            version_lock
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            manager.usage.release_scope(
                &identity.package,
                &identity.version.to_string(),
                &identity.scope,
            );
            manager.process_exited(identity.package, identity.scope);
            exit_flag.store(true, std::sync::atomic::Ordering::Release);
            completion.notify_waiters();
        });
        let candidate = ProcessSession {
            peer: peer.clone(),
            stop,
            finished,
            exited,
        };
        let handshake = peer.request("initialize", json!({"protocol":CAPABILITY_PROTOCOL_VERSION,"instance":self.identity,"configuration":self.configuration,"selected":self.selected_implementations,"contributions":contributions}), Duration::from_secs(15)).await;
        let handshake: BackendHandshake =
            match handshake.and_then(|value| Ok(serde_json::from_value(value)?)) {
                Ok(handshake) => handshake,
                Err(error) => {
                    stop_session(candidate).await;
                    return Err(error);
                }
            };
        let expected: std::collections::BTreeSet<_> = pack
            .manifest
            .implementations
            .iter()
            .map(|i| i.id.clone())
            .collect();
        if handshake.protocol != CAPABILITY_PROTOCOL_VERSION
            || handshake.package != self.identity.package
            || handshake.version != self.identity.version
            || handshake
                .implementations
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>()
                != expected
        {
            stop_session(candidate).await;
            return Err(RuntimeError::Invalid(
                "backend handshake differs from installed manifest".into(),
            ));
        }
        *session = Some(candidate);
        Ok(peer)
    }
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> RuntimeResult<Value> {
        let _lease = self.occupy("operation", method)?;
        self.peer().await?.request(method, params, timeout).await
    }
    pub async fn shutdown(&self) -> RuntimeResult<()> {
        let first = !self.retired.swap(true, std::sync::atomic::Ordering::AcqRel);
        let mut session = self.session.lock().await;
        let mut result = Ok(());
        if let Some(session) = session.take() {
            if !session.peer.is_closed() {
                result = session
                    .peer
                    .request("shutdown", Value::Null, Duration::from_secs(5))
                    .await
                    .map(|_| ());
            }
            stop_session(session).await;
        }
        self.version_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        self.state_leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.video_jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        if first {
            self.selection
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            self.manager.usage.release_scope(
                &self.identity.package,
                &self.identity.version.to_string(),
                &self.identity.scope,
            );
        }
        result
    }
}
impl Drop for BackendInstance {
    fn drop(&mut self) {
        let _ = &self.selection;
        if let Some(session) = self.session.get_mut().take() {
            let _ = session.stop.try_send(());
            session.peer.close();
        }
    }
}
async fn stop_session(session: ProcessSession) {
    let _ = session.stop.try_send(());
    session.peer.close();
    loop {
        let notified = session.finished.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if session.exited.load(std::sync::atomic::Ordering::Acquire) {
            break;
        }
        notified.await;
    }
}
async fn terminate_tree(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };
    #[cfg(unix)]
    {
        // The child starts its own process group, including stdio MCP children.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output()
            .await;
    }
}
pub fn platform_key() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}
