//! Portable tool adapter. The host supplies authorization for every call.
use std::sync::Arc;

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use eliteforge_agent_core::{
    AgentError, RunContext, Tool, ToolBinding, ToolCatalogEntry, ToolDescriptor, ToolError,
    ToolLease, ToolOperationKind, ToolOutput, ToolProvider,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{ConnectorError, ConnectorService, PageRequest, Principal};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectorCapability {
    RepositoryRead,
    TaskRead,
    TaskWrite,
    ReviewRead,
    ReviewWrite,
}

impl ConnectorCapability {
    pub fn call_name(self) -> &'static str {
        match self {
            Self::RepositoryRead => "connector_repository_read",
            Self::TaskRead => "connector_task_read",
            Self::TaskWrite => "connector_task_write",
            Self::ReviewRead => "connector_review_read",
            Self::ReviewWrite => "connector_review_write",
        }
    }
    fn actions(self) -> &'static [&'static str] {
        match self {
            Self::RepositoryRead => &["search", "get", "branches", "tree", "readFile"],
            Self::TaskRead | Self::ReviewRead => &["search", "get", "comments"],
            Self::TaskWrite | Self::ReviewWrite => &["create", "comment"],
        }
    }
    fn is_write(self) -> bool {
        matches!(self, Self::TaskWrite | Self::ReviewWrite)
    }
}

const CAPABILITIES: [ConnectorCapability; 5] = [
    ConnectorCapability::RepositoryRead,
    ConnectorCapability::TaskRead,
    ConnectorCapability::TaskWrite,
    ConnectorCapability::ReviewRead,
    ConnectorCapability::ReviewWrite,
];

/// The host decides which capabilities enter the lease and authorizes each
/// operation again against the live caller, connection and resource scope.
/// The principal must come from trusted run state, never tool arguments.
#[async_trait]
pub trait ConnectorToolAuthority: Send + Sync {
    async fn expose(
        &self,
        run: &RunContext,
        capability: ConnectorCapability,
    ) -> Result<bool, AgentError>;
    async fn authorize(
        &self,
        run: &RunContext,
        capability: ConnectorCapability,
        operation: &str,
        connection_id: &str,
        resource_scope: Option<&str>,
    ) -> Result<Principal, ConnectorError>;
}

pub struct ConnectorTools {
    service: Arc<ConnectorService>,
    authority: Arc<dyn ConnectorToolAuthority>,
}
impl ConnectorTools {
    pub fn new(service: Arc<ConnectorService>, authority: Arc<dyn ConnectorToolAuthority>) -> Self {
        Self { service, authority }
    }
}

fn descriptor(capability: ConnectorCapability) -> ToolDescriptor {
    let description = match capability {
        ConnectorCapability::RepositoryRead => {
            "Search optionally takes query; get and branches require repositoryId; tree requires repositoryId and revision, optionally path; readFile also requires path."
        }
        ConnectorCapability::TaskRead => {
            "Requires scopeId (a repository on a Git host or a project on a task platform). Search optionally takes query; get and comments require itemId."
        }
        ConnectorCapability::TaskWrite => {
            "Requires scopeId (a repository on a Git host or a project on a task platform). Create requires title and optionally description; comment requires itemId and body. Check indeterminate writes before retrying."
        }
        ConnectorCapability::ReviewRead => {
            "Requires repositoryId. Search optionally takes query; get and comments require itemId."
        }
        ConnectorCapability::ReviewWrite => {
            "Requires repositoryId. Create requires title, source and target and optionally description; comment requires itemId and body. Check indeterminate writes before retrying."
        }
    };
    let schema = json!({"type":"object", "additionalProperties":false, "required":["connectionId","operation"], "properties": {
        "connectionId":{"type":"string","minLength":1}, "operation":{"type":"string","enum":capability.actions()},
        "repositoryId":{"type":"string"}, "scopeId":{"type":"string"}, "query":{"type":"string"}, "itemId":{"type":"string"},
        "revision":{"type":"string"}, "path":{"type":"string"}, "cursor":{"type":"string"}, "limit":{"type":"integer","minimum":1,"maximum":100},
        "title":{"type":"string"}, "description":{"type":"string"}, "body":{"type":"string"}, "source":{"type":"string"}, "target":{"type":"string"}
    }});
    let descriptor = ToolDescriptor::new(
        format!("framework.{}", capability.call_name()),
        capability.call_name(),
        description,
        schema,
    )
    .with_operation_kind(if capability.is_write() {
        ToolOperationKind::Command
    } else {
        ToolOperationKind::Query
    });
    if capability.is_write() {
        descriptor.with_single_input()
    } else {
        descriptor
    }
}

pub fn connector_tool_catalog() -> Vec<ToolCatalogEntry> {
    CAPABILITIES
        .into_iter()
        .map(|capability| ToolCatalogEntry {
            descriptor: descriptor(capability),
            domain_code: Some("connectors".into()),
            category: None,
        })
        .collect()
}

#[async_trait]
impl ToolProvider for ConnectorTools {
    fn catalog_metadata(&self) -> Option<Vec<ToolCatalogEntry>> {
        Some(connector_tool_catalog())
    }
    async fn lease(&self, run: &RunContext) -> Result<ToolLease, AgentError> {
        let mut bindings = Vec::new();
        for capability in CAPABILITIES {
            if self.authority.expose(run, capability).await? {
                let mut binding = ToolBinding::new(
                    descriptor(capability),
                    ConnectorTool {
                        capability,
                        service: self.service.clone(),
                        authority: self.authority.clone(),
                    },
                )
                .with_domain_code("connectors");
                binding.parallel_safe = !capability.is_write();
                bindings.push(binding);
            }
        }
        Ok(ToolLease::new(bindings))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Input {
    connection_id: String,
    operation: String,
    repository_id: Option<String>,
    scope_id: Option<String>,
    query: Option<String>,
    item_id: Option<String>,
    revision: Option<String>,
    path: Option<String>,
    cursor: Option<String>,
    limit: Option<u32>,
    title: Option<String>,
    description: Option<String>,
    body: Option<String>,
    source: Option<String>,
    target: Option<String>,
}

impl Input {
    fn required<'a>(value: &'a Option<String>, name: &str) -> Result<&'a str, ToolError> {
        value
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| ToolError::Failed(format!("{name} is required")))
    }
    fn page(&self) -> PageRequest {
        PageRequest {
            cursor: self.cursor.clone(),
            limit: self.limit.unwrap_or(20).clamp(1, 100),
        }
    }
}

struct ConnectorTool {
    capability: ConnectorCapability,
    service: Arc<ConnectorService>,
    authority: Arc<dyn ConnectorToolAuthority>,
}

fn failed(error: impl std::fmt::Display) -> ToolError {
    ToolError::Failed(error.to_string())
}
fn output(value: impl Serialize) -> Result<ToolOutput, ToolError> {
    serde_json::to_string(&value)
        .map(ToolOutput::text)
        .map_err(failed)
}

#[async_trait]
impl Tool for ConnectorTool {
    async fn call(&self, arguments: Value, run: &RunContext) -> Result<ToolOutput, ToolError> {
        run.check_cancelled().map_err(failed)?;
        if arguments.to_string().len() > 256 * 1024 {
            return Err(failed("connector input exceeds 256 KiB"));
        }
        let input: Input = serde_json::from_value(arguments).map_err(failed)?;
        if input.connection_id.trim().is_empty()
            || !self
                .capability
                .actions()
                .contains(&input.operation.as_str())
        {
            return Err(failed("invalid connector operation"));
        }
        let principal = self
            .authority
            .authorize(
                run,
                self.capability,
                &input.operation,
                &input.connection_id,
                if matches!(
                    self.capability,
                    ConnectorCapability::TaskRead | ConnectorCapability::TaskWrite
                ) {
                    input.scope_id.as_deref()
                } else {
                    input.repository_id.as_deref()
                },
            )
            .await
            .map_err(failed)?;
        let provider = self
            .service
            .connect(&principal, &input.connection_id)
            .await
            .map_err(failed)?;
        let query = input.query.as_deref().unwrap_or("");
        let repository = || Input::required(&input.repository_id, "repositoryId");
        let scope = || Input::required(&input.scope_id, "scopeId");
        let item = || Input::required(&input.item_id, "itemId");
        match self.capability {
            ConnectorCapability::RepositoryRead => {
                let api = provider
                    .repositories()
                    .ok_or_else(|| failed(ConnectorError::Unsupported))?;
                match input.operation.as_str() {
                    "search" => output(api.search(query, input.page()).await.map_err(failed)?),
                    "get" => output(api.get(repository()?).await.map_err(failed)?),
                    "branches" => output(
                        api.branches(repository()?, input.page())
                            .await
                            .map_err(failed)?,
                    ),
                    "tree" => output(
                        api.tree(
                            repository()?,
                            Input::required(&input.revision, "revision")?,
                            input.path.as_deref().unwrap_or(""),
                            input.page(),
                        )
                        .await
                        .map_err(failed)?,
                    ),
                    "readFile" => {
                        let bytes = api
                            .read_file(
                                repository()?,
                                Input::required(&input.revision, "revision")?,
                                Input::required(&input.path, "path")?,
                            )
                            .await
                            .map_err(failed)?;
                        if bytes.len() > 512 * 1024 {
                            return Err(failed("file exceeds 512 KiB tool output limit"));
                        }
                        match std::str::from_utf8(&bytes) {
                            Ok(content) => output(json!({"encoding":"utf8", "content": content})),
                            Err(_) => output(
                                json!({"encoding":"base64", "content": STANDARD.encode(bytes)}),
                            ),
                        }
                    }
                    _ => Err(failed("invalid connector operation")),
                }
            }
            ConnectorCapability::TaskRead => {
                let api = provider
                    .tasks()
                    .ok_or_else(|| failed(ConnectorError::Unsupported))?;
                match input.operation.as_str() {
                    "search" => output(
                        api.search(scope()?, query, input.page())
                            .await
                            .map_err(failed)?,
                    ),
                    "get" => output(api.get(scope()?, item()?).await.map_err(failed)?),
                    "comments" => output(
                        api.comments(scope()?, item()?, input.page())
                            .await
                            .map_err(failed)?,
                    ),
                    _ => Err(failed("invalid connector operation")),
                }
            }
            ConnectorCapability::TaskWrite => {
                let api = provider
                    .task_writer()
                    .ok_or_else(|| failed(ConnectorError::Unsupported))?;
                match input.operation.as_str() {
                    "create" => output(
                        api.create(
                            scope()?,
                            Input::required(&input.title, "title")?,
                            input.description.as_deref().unwrap_or(""),
                        )
                        .await
                        .map_err(failed)?,
                    ),
                    "comment" => output(
                        api.comment(scope()?, item()?, Input::required(&input.body, "body")?)
                            .await
                            .map_err(failed)?,
                    ),
                    _ => Err(failed("invalid connector operation")),
                }
            }
            ConnectorCapability::ReviewRead => {
                let api = provider
                    .reviews()
                    .ok_or_else(|| failed(ConnectorError::Unsupported))?;
                match input.operation.as_str() {
                    "search" => output(
                        api.search(repository()?, query, input.page())
                            .await
                            .map_err(failed)?,
                    ),
                    "get" => output(api.get(repository()?, item()?).await.map_err(failed)?),
                    "comments" => output(
                        api.comments(repository()?, item()?, input.page())
                            .await
                            .map_err(failed)?,
                    ),
                    _ => Err(failed("invalid connector operation")),
                }
            }
            ConnectorCapability::ReviewWrite => {
                let api = provider
                    .review_writer()
                    .ok_or_else(|| failed(ConnectorError::Unsupported))?;
                match input.operation.as_str() {
                    "create" => output(
                        api.create(
                            repository()?,
                            Input::required(&input.title, "title")?,
                            input.description.as_deref().unwrap_or(""),
                            Input::required(&input.source, "source")?,
                            Input::required(&input.target, "target")?,
                        )
                        .await
                        .map_err(failed)?,
                    ),
                    "comment" => output(
                        api.comment(
                            repository()?,
                            item()?,
                            Input::required(&input.body, "body")?,
                        )
                        .await
                        .map_err(failed)?,
                    ),
                    _ => Err(failed("invalid connector operation")),
                }
            }
        }
    }
}
