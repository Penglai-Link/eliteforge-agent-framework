use crate::*;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageContext {
    pub theme: String,
    pub language: String,
    pub data: Value,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageMessage {
    pub session: String,
    pub instance: InstanceIdentity,
    pub topic: String,
    pub data: Value,
}
#[async_trait]
pub trait PageHost: Send + Sync {
    async fn call(&self, capability: &str, params: Value) -> Result<Value, RpcFault>;
}
pub struct NoPageHost;
#[async_trait]
impl PageHost for NoPageHost {
    async fn call(&self, _: &str, _: Value) -> Result<Value, RpcFault> {
        Err(RpcFault::new("page host capability not granted"))
    }
}
/// A version-pinned iframe/WebView session. The host owns mounting and transport.
pub struct PageSession {
    id: String,
    instance: Arc<BackendInstance>,
    descriptor: PageDescriptor,
    context: PageContext,
    permissions: BTreeSet<String>,
    host: Arc<dyn PageHost>,
    closed: AtomicBool,
    cancellation: CancellationToken,
    lease: Mutex<Option<UsageLease>>,
    topics: Mutex<Option<mpsc::Sender<PageMessage>>>,
}
impl PageSession {
    pub fn new(
        instance: Arc<BackendInstance>,
        page: &str,
        context: PageContext,
        grants: BTreeSet<String>,
        host: Arc<dyn PageHost>,
    ) -> RuntimeResult<(Arc<Self>, mpsc::Receiver<PageMessage>)> {
        if !instance.page_is_selected(page) {
            return Err(RuntimeError::Invalid(
                "page is not selected in this scope".into(),
            ));
        }
        let descriptor = instance
            .pack()
            .manifest
            .pages
            .iter()
            .find(|p| p.id == page)
            .cloned()
            .ok_or_else(|| RuntimeError::Invalid("unknown capability page".into()))?;
        let permissions = descriptor
            .permissions
            .iter()
            .filter(|permission| grants.contains(*permission))
            .cloned()
            .collect();
        let lease = instance.occupy("page", &descriptor.id)?;
        let (topics, receiver) = mpsc::channel(64);
        Ok((
            Arc::new(Self {
                id: uuid::Uuid::new_v4().to_string(),
                instance,
                descriptor,
                context,
                permissions,
                host,
                closed: AtomicBool::new(false),
                cancellation: CancellationToken::new(),
                lease: Mutex::new(Some(lease)),
                topics: Mutex::new(Some(topics)),
            }),
            receiver,
        ))
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn descriptor(&self) -> &PageDescriptor {
        &self.descriptor
    }
    pub fn read_resource(&self, relative: &str) -> RuntimeResult<Vec<u8>> {
        self.validate(&self.id, &self.instance.identity)?;
        let _lease = self.instance.occupy("page_resource", relative)?;
        self.validate(&self.id, &self.instance.identity)?;
        self.instance.read_resource(relative)
    }
    pub fn initialization(&self) -> Value {
        json!({"protocol":CAPABILITY_PROTOCOL_VERSION,"session":self.id,"instance":self.instance.identity,"page":self.descriptor.id,"context":self.context,"permissions":self.permissions,"topics":self.descriptor.topics})
    }
    fn validate(&self, session: &str, instance: &InstanceIdentity) -> RuntimeResult<()> {
        if self.instance.is_retired()
            || self.closed.load(Ordering::Acquire)
            || session != self.id
            || instance != &self.instance.identity
        {
            return Err(RuntimeError::Invalid(
                "closed or mismatched page session".into(),
            ));
        }
        Ok(())
    }
    pub async fn publish(&self, message: PageMessage) -> RuntimeResult<()> {
        self.validate(&message.session, &message.instance)?;
        if !self.descriptor.topics.contains(&message.topic) {
            return Err(RuntimeError::Invalid("page topic was not declared".into()));
        }
        let topics = self
            .topics
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| RuntimeError::Invalid("page closed".into()))?;
        tokio::select! { biased; _ = self.cancellation.cancelled() => Err(RuntimeError::Invalid("page closed".into())), result = topics.send(message) => result.map_err(|_| RuntimeError::Invalid("page subscriber closed".into())) }
    }
    pub async fn call(
        &self,
        session: &str,
        instance: &InstanceIdentity,
        method: &str,
        params: Value,
    ) -> RuntimeResult<Value> {
        self.validate(session, instance)?;
        let _lease = self.instance.occupy("page_operation", method)?;
        let peer = self.instance.peer().await?;
        let mut scope = peer.scope();
        let scope_id = scope.id().to_owned();
        let request = peer.request_in_scope(
            Some(&scope_id),
            "page.call",
            json!({"session":self.id,"page":self.descriptor.id,"method":method,"params":params}),
            Duration::from_secs(60),
        );
        tokio::pin!(request);
        loop {
            tokio::select! {
                biased;
                _ = self.cancellation.cancelled() => return Err(RuntimeError::Invalid("page closed".into())),
                incoming = scope.incoming.recv() => {
                    let Some(incoming) = incoming else { return Err(RuntimeError::Rpc("page callback scope closed".into())); };
                    let method = incoming.request.method.strip_prefix("host.page.");
                    let callback = async { match method {
                        Some("publish")=>self.publish(PageMessage{session:self.id.clone(),instance:self.instance.identity.clone(),topic:incoming.request.params["topic"].as_str().unwrap_or_default().into(),data:incoming.request.params["data"].clone()}).await.map(|_|Value::Null).map_err(|e|RpcFault::new(e.to_string())),
                        Some(capability) if self.permissions.contains(capability) => self.host.call(capability, incoming.request.params.clone()).await,
                        _ => Err(RpcFault::new("page capability not granted")),
                    } };
                    let result = tokio::select! { biased; _ = self.cancellation.cancelled() => return Err(RuntimeError::Invalid("page closed".into())), _ = incoming.cancelled() => Err(RpcFault::new("page callback cancelled")), result = callback => result, result = &mut request => return result };
                    incoming.respond(result).await?;
                },
                result = &mut request => return result,
            }
        }
    }
    pub fn close(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            self.cancellation.cancel();
            self.topics.lock().unwrap_or_else(|e| e.into_inner()).take();
            self.lease.lock().unwrap_or_else(|e| e.into_inner()).take();
        }
    }
}
impl Drop for PageSession {
    fn drop(&mut self) {
        self.close();
    }
}
