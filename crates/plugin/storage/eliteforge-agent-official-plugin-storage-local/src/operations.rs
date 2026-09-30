//! Storage requests submitted through `agent.execute(request)`.
use crate::{ArtifactStoreService, SqliteCapabilitiesService, SummaryStoreService};
use async_trait::async_trait;
pub use eliteforge_agent_context_contracts::{
    ArtifactChunk, ArtifactId, ArtifactMetadata, ConversationSummary, SummaryKey, VersionedSummary,
};
use eliteforge_agent_core::{AgentError, AgentOperation};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub struct ReadSqliteCapabilities;
#[async_trait]
impl AgentOperation for ReadSqliteCapabilities {
    type Service = SqliteCapabilitiesService;
    type Output = Vec<(String, crate::ExtensionReport)>;
    async fn execute(
        self,
        service: Arc<Self::Service>,
        _: CancellationToken,
    ) -> Result<Self::Output, AgentError> {
        Ok(service.0.clone())
    }
}

pub struct LoadSummary(pub SummaryKey);
pub struct SaveSummary {
    pub key: SummaryKey,
    pub expected: Option<String>,
    pub value: ConversationSummary,
}
pub struct DeleteSummary {
    pub key: SummaryKey,
    pub expected: String,
}
#[async_trait]
impl AgentOperation for LoadSummary {
    type Service = SummaryStoreService;
    type Output = Option<VersionedSummary>;
    async fn execute(
        self,
        service: Arc<Self::Service>,
        _: CancellationToken,
    ) -> Result<Self::Output, AgentError> {
        Ok(service.0.load(&self.0).await?)
    }
}
#[async_trait]
impl AgentOperation for SaveSummary {
    type Service = SummaryStoreService;
    type Output = bool;
    async fn execute(
        self,
        service: Arc<Self::Service>,
        _: CancellationToken,
    ) -> Result<Self::Output, AgentError> {
        Ok(service
            .0
            .compare_exchange(&self.key, self.expected.as_deref(), &self.value)
            .await?)
    }
}
#[async_trait]
impl AgentOperation for DeleteSummary {
    type Service = SummaryStoreService;
    type Output = bool;
    async fn execute(
        self,
        service: Arc<Self::Service>,
        _: CancellationToken,
    ) -> Result<Self::Output, AgentError> {
        Ok(service.0.delete(&self.key, &self.expected).await?)
    }
}

pub struct PutArtifact(pub Vec<u8>);
pub struct ReadArtifact {
    pub id: ArtifactId,
    pub offset: u64,
    pub length: usize,
}
pub struct RemoveArtifact(pub ArtifactId);
fn artifact_error(error: eliteforge_agent_context_contracts::ArtifactError) -> AgentError {
    match error {
        eliteforge_agent_context_contracts::ArtifactError::Cancelled => AgentError::Cancelled,
        error => AgentError::Context(eliteforge_agent_core::ContextError::Other(
            error.to_string(),
        )),
    }
}
#[async_trait]
impl AgentOperation for PutArtifact {
    type Service = ArtifactStoreService;
    type Output = ArtifactMetadata;
    async fn execute(
        self,
        service: Arc<Self::Service>,
        cancellation: CancellationToken,
    ) -> Result<Self::Output, AgentError> {
        service
            .0
            .put(self.0, cancellation)
            .await
            .map_err(artifact_error)
    }
}
#[async_trait]
impl AgentOperation for ReadArtifact {
    type Service = ArtifactStoreService;
    type Output = ArtifactChunk;
    async fn execute(
        self,
        service: Arc<Self::Service>,
        cancellation: CancellationToken,
    ) -> Result<Self::Output, AgentError> {
        service
            .0
            .read(&self.id, self.offset, self.length, cancellation)
            .await
            .map_err(artifact_error)
    }
}
#[async_trait]
impl AgentOperation for RemoveArtifact {
    type Service = ArtifactStoreService;
    type Output = ();
    async fn execute(
        self,
        service: Arc<Self::Service>,
        cancellation: CancellationToken,
    ) -> Result<(), AgentError> {
        service
            .0
            .remove(&self.0, cancellation)
            .await
            .map_err(artifact_error)
    }
}
