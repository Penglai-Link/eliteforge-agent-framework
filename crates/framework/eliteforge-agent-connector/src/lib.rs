//! Host-independent contracts for connecting external services.
//!
//! A host authorizes and resolves a connection for each caller. Providers own
//! platform-specific behavior; portable operations are exposed as independent
//! capabilities so that implementations need not pretend to support them all.

use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use eliteforge_agent_core::{Plugin, PluginContext, PluginError, extension_point};
use eliteforge_agent_plugin_core::ServiceRegistry;
use serde::{Deserialize, Serialize};

mod tools;
pub use tools::{
    ConnectorCapability, ConnectorToolAuthority, ConnectorTools, connector_tool_catalog,
};

/// Stable, host-defined identity for a single invocation's caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    pub id: String,
    pub scope: String,
}

/// Public connection metadata. Credentials are supplied separately by the host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Connection {
    pub id: String,
    pub provider: String,
    pub endpoint: String,
}

/// Secret material is deliberately excluded from serialization and debugging.
pub struct Credential(Vec<u8>);

impl Credential {
    pub fn new(value: Vec<u8>) -> Self {
        Self(value)
    }

    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

/// The host must authorize the principal before returning either value.
#[async_trait]
pub trait ConnectionResolver: Send + Sync {
    async fn resolve(
        &self,
        principal: &Principal,
        connection_id: &str,
    ) -> Result<(Connection, Credential), ConnectorError>;
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ConnectorError {
    #[error("connection not found")]
    NotFound,
    #[error("access denied")]
    Forbidden,
    #[error("authentication failed")]
    Authentication,
    #[error("operation is not supported")]
    Unsupported,
    #[error("remote service is unavailable")]
    Unavailable,
    #[error("remote service rate limit exceeded")]
    RateLimited,
    #[error("remote response was invalid")]
    InvalidResponse,
    #[error("remote service rejected the request")]
    Rejected,
    #[error("the outcome of the write is unknown")]
    IndeterminateWrite,
    #[error("connector configuration is invalid")]
    Configuration,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDescriptor {
    pub id: String,
    pub display_name: String,
}

/// Register provider contributions through the existing Framework registry.
#[extension_point(contribute, adapter = ConnectorProviderPlugin)]
#[async_trait]
pub trait ConnectorProvider: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;
    async fn connect(
        &self,
        connection: &Connection,
        credential: Credential,
    ) -> Result<Arc<dyn ConnectedProvider>, ConnectorError>;
}

pub struct ConnectorProviderPlugin {
    id: String,
    provider: Arc<dyn ConnectorProvider>,
}

impl ConnectorProviderPlugin {
    pub fn from_arc(id: impl Into<String>, provider: Arc<dyn ConnectorProvider>) -> Self {
        Self {
            id: id.into(),
            provider,
        }
    }
}

#[async_trait]
impl Plugin for ConnectorProviderPlugin {
    fn id(&self) -> &str {
        &self.id
    }

    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        context.contribute_extension(self.provider.clone())
    }
}

/// Capability presence describes provider support, not authorization to access
/// any particular remote resource. Every operation can still return Forbidden.
#[async_trait]
pub trait ConnectedProvider: Send + Sync {
    /// Check connectivity and credentials against the remote service.
    async fn check(&self) -> Result<(), ConnectorError>;
    fn repositories(&self) -> Option<Arc<dyn RepositoryReader>> {
        None
    }
    fn tasks(&self) -> Option<Arc<dyn TaskReader>> {
        None
    }
    fn task_writer(&self) -> Option<Arc<dyn TaskWriter>> {
        None
    }
    fn reviews(&self) -> Option<Arc<dyn ReviewReader>> {
        None
    }
    fn review_writer(&self) -> Option<Arc<dyn ReviewWriter>> {
        None
    }
}

/// Coordinates provider discovery and host-authorized connection resolution.
pub struct ConnectorService {
    providers: HashMap<String, Arc<dyn ConnectorProvider>>,
    resolver: Arc<dyn ConnectionResolver>,
}

impl ConnectorService {
    pub fn new(
        providers: Vec<Arc<dyn ConnectorProvider>>,
        resolver: Arc<dyn ConnectionResolver>,
    ) -> Result<Self, ConnectorError> {
        let mut by_id = HashMap::new();
        for provider in providers {
            let id = provider.descriptor().id;
            if id.trim().is_empty() || by_id.insert(id, provider).is_some() {
                return Err(ConnectorError::Configuration);
            }
        }
        Ok(Self {
            providers: by_id,
            resolver,
        })
    }

    pub fn from_registry(
        registry: &ServiceRegistry,
        resolver: Arc<dyn ConnectionResolver>,
    ) -> Result<Self, ConnectorError> {
        Self::new(registry.extensions::<dyn ConnectorProvider>(), resolver)
    }

    pub fn providers(&self) -> Vec<ProviderDescriptor> {
        let mut providers: Vec<_> = self.providers.values().map(|p| p.descriptor()).collect();
        providers.sort_by(|a, b| a.id.cmp(&b.id));
        providers
    }

    pub async fn connect(
        &self,
        principal: &Principal,
        connection_id: &str,
    ) -> Result<Arc<dyn ConnectedProvider>, ConnectorError> {
        let (connection, credential) = self.resolver.resolve(principal, connection_id).await?;
        if connection.id != connection_id {
            return Err(ConnectorError::Configuration);
        }
        self.providers
            .get(&connection.provider)
            .ok_or(ConnectorError::Unsupported)?
            .connect(&connection, credential)
            .await
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageRequest {
    pub cursor: Option<String>,
    pub limit: u32,
}

impl Default for PageRequest {
    fn default() -> Self {
        Self {
            cursor: None,
            limit: 20,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

/// IDs remain provider-owned and are interpreted together with connection ID.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Repository {
    pub id: String,
    pub name: String,
    pub full_name: String,
    pub web_url: String,
    pub clone_url: Option<String>,
    pub ssh_url: Option<String>,
    pub default_branch: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TreeEntry {
    pub path: String,
    pub is_directory: bool,
}

#[async_trait]
pub trait RepositoryReader: Send + Sync {
    async fn search(
        &self,
        query: &str,
        page: PageRequest,
    ) -> Result<Page<Repository>, ConnectorError>;
    async fn get(&self, id: &str) -> Result<Repository, ConnectorError>;
    async fn branches(&self, id: &str, page: PageRequest) -> Result<Page<String>, ConnectorError>;
    async fn tree(
        &self,
        id: &str,
        revision: &str,
        path: &str,
        page: PageRequest,
    ) -> Result<Page<TreeEntry>, ConnectorError>;
    async fn read_file(
        &self,
        id: &str,
        revision: &str,
        path: &str,
    ) -> Result<Vec<u8>, ConnectorError>;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub state: String,
    pub web_url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Comment {
    pub id: String,
    pub body: String,
    pub author: String,
}

#[async_trait]
/// `scope` is provider-owned: a repository for Git hosts or a project/board
/// for a task-management platform. It is always interpreted with a connection.
pub trait TaskReader: Send + Sync {
    async fn search(
        &self,
        scope: &str,
        query: &str,
        page: PageRequest,
    ) -> Result<Page<Item>, ConnectorError>;
    async fn get(&self, scope: &str, id: &str) -> Result<Item, ConnectorError>;
    async fn comments(
        &self,
        scope: &str,
        id: &str,
        page: PageRequest,
    ) -> Result<Page<Comment>, ConnectorError>;
}

#[async_trait]
pub trait TaskWriter: Send + Sync {
    async fn create(
        &self,
        scope: &str,
        title: &str,
        description: &str,
    ) -> Result<Item, ConnectorError>;
    async fn comment(&self, scope: &str, id: &str, body: &str) -> Result<Comment, ConnectorError>;
}

#[async_trait]
pub trait ReviewReader: Send + Sync {
    async fn search(
        &self,
        repository: &str,
        query: &str,
        page: PageRequest,
    ) -> Result<Page<Item>, ConnectorError>;
    async fn get(&self, repository: &str, id: &str) -> Result<Item, ConnectorError>;
    async fn comments(
        &self,
        repository: &str,
        id: &str,
        page: PageRequest,
    ) -> Result<Page<Comment>, ConnectorError>;
}

#[async_trait]
pub trait ReviewWriter: Send + Sync {
    async fn create(
        &self,
        repository: &str,
        title: &str,
        description: &str,
        source: &str,
        target: &str,
    ) -> Result<Item, ConnectorError>;
    async fn comment(
        &self,
        repository: &str,
        id: &str,
        body: &str,
    ) -> Result<Comment, ConnectorError>;
}
