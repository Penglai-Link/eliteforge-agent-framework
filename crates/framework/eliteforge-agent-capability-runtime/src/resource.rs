use crate::*;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteResourceHandle {
    pub id: String,
    pub implementation: String,
}
#[async_trait]
pub trait BackendResource: Send + Sync {
    async fn call(
        &self,
        operation: &str,
        params: Value,
        context: BackendCallContext,
    ) -> Result<Value, RpcFault>;
    async fn close(&self) -> Result<(), RpcFault>;
}
type OwnedBackendResource = (String, Arc<dyn BackendResource>);
#[derive(Default)]
pub struct BackendResources {
    resources: Mutex<BTreeMap<String, OwnedBackendResource>>,
}
impl BackendResources {
    pub async fn close_all(&self) -> Result<(), RpcFault> {
        let resources =
            std::mem::take(&mut *self.resources.lock().unwrap_or_else(|e| e.into_inner()));
        let mut failures = Vec::new();
        for (_, (_, resource)) in resources {
            if let Err(error) = resource.close().await {
                failures.push(error.message);
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(RpcFault::new(failures.join("; ")))
        }
    }
    pub(crate) fn discard(&self, ids: &[String]) -> Vec<Arc<dyn BackendResource>> {
        let mut resources = self.resources.lock().unwrap_or_else(|e| e.into_inner());
        ids.iter()
            .filter_map(|id| resources.remove(id).map(|(_, resource)| resource))
            .collect()
    }
    pub fn insert(
        &self,
        implementation: &str,
        resource: Arc<dyn BackendResource>,
    ) -> Result<RemoteResourceHandle, RpcFault> {
        let mut resources = self.resources.lock().unwrap_or_else(|e| e.into_inner());
        if resources.len() >= 256 {
            return Err(RpcFault::new("remote resource capacity exceeded"));
        }
        let id = uuid::Uuid::new_v4().to_string();
        resources.insert(id.clone(), (implementation.into(), resource));
        Ok(RemoteResourceHandle {
            id,
            implementation: implementation.into(),
        })
    }
    pub async fn call(
        &self,
        implementation: &str,
        operation: &str,
        params: Value,
        context: BackendCallContext,
    ) -> Result<Value, RpcFault> {
        let handle: RemoteResourceHandle = serde_json::from_value(params["handle"].clone())
            .map_err(|e| RpcFault::new(e.to_string()))?;
        let resource = {
            let mut resources = self.resources.lock().unwrap_or_else(|e| e.into_inner());
            let (owner, resource) = resources
                .get(&handle.id)
                .ok_or_else(|| RpcFault::new("remote handle closed or unknown"))?;
            if owner != implementation || handle.implementation != implementation {
                return Err(RpcFault::new("remote handle ownership mismatch"));
            }
            if operation == "release" {
                resources.remove(&handle.id).expect("validated resource").1
            } else {
                resource.clone()
            }
        };
        if operation == "release" {
            resource.close().await?;
            Ok(Value::Null)
        } else {
            resource
                .call(operation, params["params"].clone(), context)
                .await
        }
    }
}
/// Host-owned, version-bound lease. Drop releases once; a backend crash never replays it.
pub struct RemoteResource {
    implementation: RemoteImplementation,
    handle: RemoteResourceHandle,
    closed: AtomicBool,
    lease: Mutex<Option<UsageLease>>,
}
impl RemoteResource {
    pub fn handle(&self) -> &RemoteResourceHandle {
        &self.handle
    }
    pub fn new(
        implementation: RemoteImplementation,
        handle: RemoteResourceHandle,
    ) -> RuntimeResult<Arc<Self>> {
        if handle.implementation != implementation.declaration.id {
            return Err(RuntimeError::Invalid(
                "remote resource implementation mismatch".into(),
            ));
        }
        let lease = implementation
            .instance
            .occupy("remote_resource", &handle.id)?;
        Ok(Arc::new(Self {
            implementation,
            handle,
            closed: AtomicBool::new(false),
            lease: Mutex::new(Some(lease)),
        }))
    }
    pub async fn call(&self, operation: &str, params: Value) -> RuntimeResult<Value> {
        if self.closed.load(Ordering::Acquire) {
            return Err(RuntimeError::Invalid("remote resource closed".into()));
        }
        self.implementation
            .invoke(
                &format!("resource.{operation}"),
                json!({"handle":self.handle,"params":params}),
            )
            .await
    }
    pub async fn close(&self) -> RuntimeResult<()> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let result = self
            .implementation
            .invoke("resource.release", json!({"handle":self.handle}))
            .await;
        self.lease.lock().unwrap_or_else(|e| e.into_inner()).take();
        result.map(|_| ())
    }
}
impl Drop for RemoteResource {
    fn drop(&mut self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            let implementation = self.implementation.clone();
            let handle = self.handle.clone();
            let lease = self
                .lease
                .get_mut()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _lease = lease;
                    let _ = implementation
                        .invoke("resource.release", json!({"handle":handle}))
                        .await;
                });
            }
        }
    }
}
