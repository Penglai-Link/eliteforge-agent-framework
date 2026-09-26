//! Rust sidecar SDK. stdout is reserved for protocol frames; telemetry stays local.
use crate::*;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[derive(Clone)]
pub struct BackendCallContext {
    pub trace_context: Option<eliteforge_agent_core::TraceContext>,
    peer: RpcPeer,
    scope: Option<String>,
    resources: Arc<BackendResources>,
    implementation: String,
    allocated: Arc<std::sync::Mutex<Vec<String>>>,
}
impl BackendCallContext {
    pub fn register_resource(
        &self,
        resource: Arc<dyn BackendResource>,
    ) -> Result<RemoteResourceHandle, RpcFault> {
        let handle = self.resources.insert(&self.implementation, resource)?;
        self.allocated
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(handle.id.clone());
        Ok(handle)
    }
    pub async fn host_call(&self, method: &str, params: Value) -> Result<Value, RpcFault> {
        let scope = self
            .scope
            .as_deref()
            .ok_or_else(|| RpcFault::new("no host scope on this call"))?;
        if !method.starts_with("host.") {
            return Err(RpcFault::new("host call must use the host namespace"));
        }
        self.peer
            .request_in_scope(Some(scope), method, params, Duration::from_secs(60))
            .await
            .map_err(RpcFault::from)
    }
}
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BackendContributions {
    pub implementations: Vec<String>,
    pub plugins: Vec<String>,
    pub pages: Vec<String>,
}
#[async_trait]
pub trait CapabilityBackend: Send + Sync {
    /// Runs before initialize; only these contributions may acquire runtime resources.
    async fn select_contributions(&self, _selection: BackendContributions) -> Result<(), RpcFault> {
        Ok(())
    }
    async fn shutdown(&self) -> Result<(), RpcFault> {
        Ok(())
    }
    async fn initialize(
        &self,
        instance: InstanceIdentity,
        configuration: Value,
        selected: Vec<String>,
    ) -> Result<(), RpcFault>;
    async fn call(
        &self,
        implementation: &str,
        operation: &str,
        params: Value,
        context: BackendCallContext,
    ) -> Result<Value, RpcFault>;
    async fn page(
        &self,
        _page: &str,
        _method: &str,
        _params: Value,
        _context: BackendCallContext,
    ) -> Result<Value, RpcFault> {
        Err(RpcFault::new("unknown page method"))
    }
}
pub struct BackendServer {
    manifest: CapabilityPackManifest,
    backend: Arc<dyn CapabilityBackend>,
    initialized: AtomicBool,
    closed: AtomicBool,
    resources: Arc<BackendResources>,
    selected: std::sync::Mutex<std::collections::BTreeSet<String>>,
    initialization: tokio::sync::Mutex<()>,
    pages: std::sync::Mutex<std::collections::BTreeSet<String>>,
}
impl BackendServer {
    pub fn new(
        manifest: CapabilityPackManifest,
        backend: Arc<dyn CapabilityBackend>,
    ) -> RuntimeResult<Self> {
        manifest.validate()?;
        Ok(Self {
            manifest,
            backend,
            initialized: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            resources: Arc::new(BackendResources::default()),
            selected: std::sync::Mutex::new(Default::default()),
            initialization: tokio::sync::Mutex::new(()),
            pages: std::sync::Mutex::new(Default::default()),
        })
    }
    pub async fn serve(self) -> RuntimeResult<()> {
        serve_backend(Arc::new(self)).await
    }
}
fn field(params: &Value, name: &str) -> Result<String, RpcFault> {
    params
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| RpcFault::new(format!("missing string field: {name}")))
}
#[async_trait]
impl RpcHandler for BackendServer {
    async fn handle(&self, request: RpcRequest, peer: RpcPeer) -> Result<Value, RpcFault> {
        if request.method == "initialize" {
            let _guard = self.initialization.lock().await;
            if self.closed.load(Ordering::Acquire) {
                return Err(RpcFault::new("backend has shut down"));
            }
            if self.initialized.load(Ordering::Acquire) {
                return Err(RpcFault::new("backend already initialized"));
            }
            let instance: InstanceIdentity =
                serde_json::from_value(request.params["instance"].clone())
                    .map_err(|error| RpcFault::new(error.to_string()))?;
            if request.params["protocol"] != CAPABILITY_PROTOCOL_VERSION
                || instance.package != self.manifest.id
                || instance.version != self.manifest.version
            {
                return Err(RpcFault::new("incompatible host handshake"));
            }
            let selected: Vec<String> = serde_json::from_value(request.params["selected"].clone())
                .map_err(|e| RpcFault::new(e.to_string()))?;
            if selected
                .iter()
                .any(|id| !self.manifest.implementations.iter().any(|i| &i.id == id))
            {
                return Err(RpcFault::new("selected implementation is undeclared"));
            }
            let contributions: BackendContributions =
                serde_json::from_value(request.params["contributions"].clone())
                    .map_err(|e| RpcFault::new(e.to_string()))?;
            if contributions.implementations != selected
                || contributions
                    .plugins
                    .iter()
                    .any(|id| !self.manifest.plugins.contains(id))
                || contributions
                    .pages
                    .iter()
                    .any(|id| !self.manifest.pages.iter().any(|p| &p.id == id))
            {
                return Err(RpcFault::new("undeclared contribution selection"));
            }
            self.backend
                .select_contributions(contributions.clone())
                .await?;
            self.backend
                .initialize(
                    instance,
                    request.params["configuration"].clone(),
                    selected.clone(),
                )
                .await?;
            *self.selected.lock().unwrap_or_else(|e| e.into_inner()) =
                selected.into_iter().collect();
            *self.pages.lock().unwrap_or_else(|e| e.into_inner()) =
                contributions.pages.into_iter().collect();
            self.initialized.store(true, Ordering::Release);
            return Ok(json!(BackendHandshake {
                protocol: CAPABILITY_PROTOCOL_VERSION,
                package: self.manifest.id.clone(),
                version: self.manifest.version.clone(),
                implementations: self
                    .manifest
                    .implementations
                    .iter()
                    .map(|i| i.id.clone())
                    .collect()
            }));
        }
        if !self.initialized.load(Ordering::Acquire) {
            return Err(RpcFault::new("backend has not initialized"));
        }
        if request.method == "shutdown" {
            let _guard = self.initialization.lock().await;
            if self.closed.swap(true, Ordering::AcqRel) {
                return Err(RpcFault::new("backend has shut down"));
            }
            self.initialized.store(false, Ordering::Release);
            let resources = self.resources.close_all().await;
            let backend = self.backend.shutdown().await;
            resources?;
            backend?;
            return Ok(Value::Null);
        }
        let allocated = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut allocation = AllocationGuard {
            resources: self.resources.clone(),
            allocated: allocated.clone(),
            committed: false,
        };
        let mut context = BackendCallContext {
            trace_context: serde_json::from_value(request.params["trace_context"].clone()).ok(),
            peer,
            scope: request.scope,
            resources: self.resources.clone(),
            implementation: String::new(),
            allocated,
        };
        let result = match request.method.as_str() {
            "extension.call" => {
                let implementation = field(&request.params, "implementation")?;
                if !self
                    .selected
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(&implementation)
                {
                    return Err(RpcFault::new("unknown implementation"));
                }
                context.implementation = implementation.clone();
                let operation = field(&request.params, "operation")?;
                if let Some(operation) = operation.strip_prefix("resource.") {
                    return self
                        .resources
                        .call(
                            &implementation,
                            operation,
                            request.params["params"].clone(),
                            context,
                        )
                        .await;
                }
                self.backend
                    .call(
                        &implementation,
                        &operation,
                        request.params["params"].clone(),
                        context,
                    )
                    .await
            }
            "page.call" => {
                let page = field(&request.params, "page")?;
                if !self
                    .pages
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(&page)
                {
                    return Err(RpcFault::new("unknown page"));
                }
                self.backend
                    .page(
                        &page,
                        &field(&request.params, "method")?,
                        request.params["params"].clone(),
                        context,
                    )
                    .await
            }
            _ => Err(RpcFault::new("unsupported backend method")),
        };
        allocation.committed = result.is_ok();
        result
    }
}

struct AllocationGuard {
    resources: Arc<BackendResources>,
    allocated: Arc<std::sync::Mutex<Vec<String>>>,
    committed: bool,
}
impl Drop for AllocationGuard {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let ids = std::mem::take(&mut *self.allocated.lock().unwrap_or_else(|e| e.into_inner()));
        for resource in self.resources.discard(&ids) {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = resource.close().await;
                });
            }
        }
    }
}
