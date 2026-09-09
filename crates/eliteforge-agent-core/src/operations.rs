//! Plugin operations are dispatched by Agent against installed services.
use crate::*;
use async_trait::async_trait;
use std::{
    any::{Any, TypeId},
    collections::HashMap,
    sync::Arc,
};
use tokio_util::sync::CancellationToken;

/// A typed request, not a plugin instance. Agent supplies the installed service
/// and owns admission, cancellation and shutdown ordering.
/// The cancellation token covers this invocation: completion, caller drop and
/// Agent shutdown all cancel it. Detached work must observe that token and clean
/// up its resources; returning a value does not extend the invocation lifetime.
#[async_trait]
pub trait AgentOperation: Send {
    type Service: Send + Sync + 'static;
    type Output: Send;
    /// Multi-contribution operations can select an explicitly named binding.
    fn select(
        &self,
        mut services: Vec<Arc<Self::Service>>,
    ) -> Result<Arc<Self::Service>, AgentError> {
        match services.len() {
            0 => Err(AgentError::MissingService(std::any::type_name::<
                Self::Service,
            >())),
            1 => Ok(services.remove(0)),
            _ => Err(AgentError::InvalidConfiguration(format!(
                "ambiguous operation service: {}",
                std::any::type_name::<Self::Service>()
            ))),
        }
    }
    async fn execute(
        self,
        service: Arc<Self::Service>,
        cancellation: CancellationToken,
    ) -> Result<Self::Output, AgentError>;
}

/// Per-execution outputs. Values are owned once, so streams cannot accidentally
/// be shared across runs or taken twice.
#[derive(Default)]
pub struct RunOutputs(HashMap<TypeId, Box<dyn Any + Send>>);
impl RunOutputs {
    pub fn contains_type(&self, output_type: TypeId) -> bool {
        self.0.contains_key(&output_type)
    }
    pub fn insert<T: Send + 'static>(&mut self, output: T) -> Result<(), AgentError> {
        let key = TypeId::of::<T>();
        if self.0.contains_key(&key) {
            return Err(AgentError::InvalidConfiguration(format!(
                "duplicate run output: {}",
                std::any::type_name::<T>()
            )));
        }
        self.0.insert(key, Box::new(output));
        Ok(())
    }
    pub fn take<T: Send + 'static>(&mut self) -> Result<T, AgentError> {
        self.0
            .remove(&TypeId::of::<T>())
            .and_then(|value| value.downcast::<T>().ok())
            .map(|value| *value)
            .ok_or_else(|| {
                AgentError::InvalidConfiguration(format!(
                    "run output not installed or already taken: {}",
                    std::any::type_name::<T>()
                ))
            })
    }
}

/// Installed by presentation plugins. Agent calls prepare before publishing any
/// event. It must be synchronous, perform no I/O and start no business work.
pub trait RunOutputFactory: Send + Sync {
    fn output_type(&self) -> TypeId;
    fn prepare(
        &self,
        request: &RunRequest,
        execution_id: RunId,
        extensions: &mut Extensions,
        outputs: &mut RunOutputs,
    ) -> Result<(), AgentError>;
}
pub struct RunOutputService(pub Arc<dyn RunOutputFactory>);

pub struct PendingInteractions(pub String);
pub struct LoadCheckpoint(pub RunId);
pub struct LatestCheckpoint(pub String);
#[async_trait]
impl AgentOperation for LoadCheckpoint {
    type Service = StateStoreService;
    type Output = Checkpoint;
    async fn execute(
        self,
        service: Arc<Self::Service>,
        _: CancellationToken,
    ) -> Result<Self::Output, AgentError> {
        Ok(service.0.load(self.0).await?)
    }
}
#[async_trait]
impl AgentOperation for LatestCheckpoint {
    type Service = StateStoreService;
    type Output = Option<Checkpoint>;
    async fn execute(
        self,
        service: Arc<Self::Service>,
        _: CancellationToken,
    ) -> Result<Self::Output, AgentError> {
        Ok(service.0.latest(&self.0).await?)
    }
}
pub struct GetInteraction {
    pub session_id: String,
    pub interaction_id: RunId,
}
#[async_trait]
impl AgentOperation for GetInteraction {
    type Service = InteractionStoreService;
    type Output = InteractionRecord;
    async fn execute(
        self,
        service: Arc<Self::Service>,
        _: CancellationToken,
    ) -> Result<Self::Output, AgentError> {
        Ok(service.0.get(&self.session_id, self.interaction_id).await?)
    }
}
#[async_trait]
impl AgentOperation for PendingInteractions {
    type Service = InteractionStoreService;
    type Output = Vec<InteractionRecord>;
    async fn execute(
        self,
        service: Arc<Self::Service>,
        _: CancellationToken,
    ) -> Result<Self::Output, AgentError> {
        InteractionManager(service.0.clone()).pending(&self.0).await
    }
}
pub struct AnswerInteraction {
    pub session_id: String,
    pub interaction_id: RunId,
    pub value: serde_json::Value,
}
#[async_trait]
impl AgentOperation for AnswerInteraction {
    type Service = InteractionStoreService;
    type Output = RunRequest;
    async fn execute(
        self,
        service: Arc<Self::Service>,
        _: CancellationToken,
    ) -> Result<Self::Output, AgentError> {
        InteractionManager(service.0.clone())
            .answer(&self.session_id, self.interaction_id, self.value)
            .await
    }
}

pub struct ReadConversation(pub ConversationKey);
#[async_trait]
impl AgentOperation for ReadConversation {
    type Service = ConversationStoreService;
    type Output = ConversationSnapshot;
    async fn execute(
        self,
        service: Arc<Self::Service>,
        _: CancellationToken,
    ) -> Result<Self::Output, AgentError> {
        let mut session = service.0.open_branch(&self.0).await?;
        let result = session.snapshot().await.map_err(AgentError::from);
        let outcome = if result.is_ok() {
            ConversationOutcome::Completed
        } else {
            ConversationOutcome::Failed
        };
        finish_with_cleanup(result, session.finish(outcome).await.map_err(Into::into))
    }
}
pub struct AppendConversation {
    pub key: ConversationKey,
    pub revision: u64,
    pub messages: Vec<Message>,
}
#[async_trait]
impl AgentOperation for AppendConversation {
    type Service = ConversationStoreService;
    type Output = ();
    async fn execute(
        self,
        service: Arc<Self::Service>,
        _: CancellationToken,
    ) -> Result<(), AgentError> {
        let mut session = service.0.open_branch(&self.key).await?;
        let result = session
            .append_at(self.revision, &self.messages)
            .await
            .map_err(Into::into);
        let outcome = if result.is_ok() {
            ConversationOutcome::Completed
        } else {
            ConversationOutcome::Failed
        };
        finish_with_cleanup(result, session.finish(outcome).await.map_err(Into::into))
    }
}

pub struct GetMemory {
    pub namespace: String,
    pub id: String,
}
pub struct SearchMemory {
    pub namespace: String,
    pub query: MemoryQuery,
}
pub struct SaveMemory {
    pub namespace: String,
    pub value: MemoryWrite,
    pub expected: Option<String>,
}
pub struct DeleteMemory {
    pub namespace: String,
    pub id: String,
    pub expected: String,
}
#[async_trait]
impl AgentOperation for GetMemory {
    type Service = LongTermMemoryService;
    type Output = MemoryEntry;
    async fn execute(
        self,
        service: Arc<Self::Service>,
        _: CancellationToken,
    ) -> Result<Self::Output, AgentError> {
        Ok(service.0.get(&self.namespace, &self.id).await?)
    }
}
#[async_trait]
impl AgentOperation for SearchMemory {
    type Service = LongTermMemoryService;
    type Output = Vec<MemoryEntry>;
    async fn execute(
        self,
        service: Arc<Self::Service>,
        _: CancellationToken,
    ) -> Result<Self::Output, AgentError> {
        Ok(service.0.search(&self.namespace, &self.query).await?)
    }
}
#[async_trait]
impl AgentOperation for SaveMemory {
    type Service = LongTermMemoryService;
    type Output = MemoryEntry;
    async fn execute(
        self,
        service: Arc<Self::Service>,
        _: CancellationToken,
    ) -> Result<Self::Output, AgentError> {
        Ok(service
            .0
            .save(&self.namespace, &self.value, self.expected.as_deref())
            .await?)
    }
}
#[async_trait]
impl AgentOperation for DeleteMemory {
    type Service = LongTermMemoryService;
    type Output = ();
    async fn execute(
        self,
        service: Arc<Self::Service>,
        _: CancellationToken,
    ) -> Result<(), AgentError> {
        Ok(service
            .0
            .delete(&self.namespace, &self.id, &self.expected)
            .await?)
    }
}
