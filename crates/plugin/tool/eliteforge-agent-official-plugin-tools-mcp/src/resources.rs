//! Resource operations share the connection's cancellation, timeout and size limits.
use super::*;
use rmcp::model::{
    ListResourceTemplatesRequest, ListResourceTemplatesResult, ListResourcesRequest,
    ListResourcesResult, ReadResourceRequest, ReadResourceRequestParams, ReadResourceResult,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

impl McpClient {
    pub async fn list_resources(
        &self,
        cursor: Option<String>,
        cancellation: &CancellationToken,
    ) -> Result<ListResourcesResult, McpError> {
        self.validate_resource_input(cursor.as_deref())?;
        let params = cursor
            .clone()
            .map(|cursor| PaginatedRequestParams::default().with_cursor(Some(cursor)));
        let request = params.map_or_else(
            ListResourcesRequest::default,
            ListResourcesRequest::with_param,
        );
        let ServerResult::ListResourcesResult(page) = self
            .request(request.into(), cancellation.cancelled())
            .await?
        else {
            return Err(McpError::Protocol);
        };
        self.validate_resource_page(
            &page,
            page.resources.len(),
            cursor.as_deref(),
            page.next_cursor.as_deref(),
        )?;
        Ok(page)
    }

    pub async fn list_resource_templates(
        &self,
        cursor: Option<String>,
        cancellation: &CancellationToken,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        self.validate_resource_input(cursor.as_deref())?;
        let params = cursor
            .clone()
            .map(|cursor| PaginatedRequestParams::default().with_cursor(Some(cursor)));
        let request = params.map_or_else(
            ListResourceTemplatesRequest::default,
            ListResourceTemplatesRequest::with_param,
        );
        let ServerResult::ListResourceTemplatesResult(page) = self
            .request(request.into(), cancellation.cancelled())
            .await?
        else {
            return Err(McpError::Protocol);
        };
        self.validate_resource_page(
            &page,
            page.resource_templates.len(),
            cursor.as_deref(),
            page.next_cursor.as_deref(),
        )?;
        Ok(page)
    }

    pub async fn read_resource(
        &self,
        uri: String,
        cancellation: &CancellationToken,
    ) -> Result<ReadResourceResult, McpError> {
        self.validate_resource_input(Some(&uri))?;
        let request = ReadResourceRequest::new(ReadResourceRequestParams::new(uri));
        let ServerResult::ReadResourceResult(result) = self
            .request(request.into(), cancellation.cancelled())
            .await?
        else {
            return Err(McpError::Protocol);
        };
        self.check_resource_size(&result)?;
        Ok(result)
    }

    fn validate_resource_input(&self, input: Option<&str>) -> Result<(), McpError> {
        if input
            .is_some_and(|input| input.is_empty() || input.len() > self.0.config.max_result_bytes)
        {
            return Err(McpError::Configuration);
        }
        Ok(())
    }

    fn check_resource_size(&self, result: &impl Serialize) -> Result<(), McpError> {
        if serde_json::to_vec(result)
            .map_err(|_| McpError::Protocol)?
            .len()
            > self.0.config.max_result_bytes
        {
            return Err(McpError::ResultLimit);
        }
        Ok(())
    }

    fn validate_resource_page(
        &self,
        page: &impl Serialize,
        count: usize,
        previous: Option<&str>,
        next: Option<&str>,
    ) -> Result<(), McpError> {
        self.check_resource_size(page)?;
        if count > self.0.config.max_tools
            || next.is_some_and(|next| {
                Some(next) == previous
                    || next.is_empty()
                    || next.len() > self.0.config.max_result_bytes
            })
        {
            return Err(McpError::DiscoveryLimit);
        }
        Ok(())
    }

    pub(crate) fn resource_bindings(&self) -> Vec<ToolBinding> {
        if self
            .0
            .peer
            .peer_info()
            .is_none_or(|info| info.capabilities.resources.is_none())
        {
            return Vec::new();
        }
        [ResourceOperation::List, ResourceOperation::Templates, ResourceOperation::Read].into_iter().map(|operation| {
            let (name, description) = match operation {
                ResourceOperation::List => ("list", "List one page of resources from this MCP server. Continue with nextCursor when returned."),
                ResourceOperation::Templates => ("templates", "List one page of resource URI templates from this MCP server. Continue with nextCursor when returned."),
                ResourceOperation::Read => ("read", "Read a resource URI from this MCP server. Use a listed resource or instantiate a listed URI template. This does not read local host files directly."),
            };
            let schema = if matches!(operation, ResourceOperation::Read) {
                json!({"type":"object", "additionalProperties":false, "required":["uri"], "properties":{"uri":{"type":"string","minLength":1}}})
            } else {
                json!({"type":"object", "additionalProperties":false, "properties":{"cursor":{"type":"string","minLength":1}}})
            };
            ToolBinding::new(ToolDescriptor::new(
                format!("mcp.resource/{}/{name}", self.0.config.namespace),
                format!("mcp_resources__{}__{name}", self.0.config.namespace),
                description, schema,
            ).with_operation_kind(eliteforge_agent_core::ToolOperationKind::Query), ResourceTool { client: self.clone(), operation }).with_source(eliteforge_agent_core::ToolSource {
                provider_id: Some(format!("mcp/{}", self.0.config.namespace)),
                ..Default::default()
            })
        }).collect()
    }
}

#[derive(Clone, Copy)]
enum ResourceOperation {
    List,
    Templates,
    Read,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListInput {
    cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadInput {
    uri: String,
}

struct ResourceTool {
    client: McpClient,
    operation: ResourceOperation,
}

#[async_trait]
impl Tool for ResourceTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let action = match self.operation {
            ResourceOperation::List | ResourceOperation::Templates => {
                let input: ListInput = serde_json::from_value(arguments)
                    .map_err(|e| ToolError::Failed(e.to_string()))?;
                if matches!(self.operation, ResourceOperation::List) {
                    McpResourceAction::List {
                        cursor: input.cursor,
                    }
                } else {
                    McpResourceAction::Templates {
                        cursor: input.cursor,
                    }
                }
            }
            ResourceOperation::Read => {
                let input: ReadInput = serde_json::from_value(arguments)
                    .map_err(|e| ToolError::Failed(e.to_string()))?;
                McpResourceAction::Read { uri: input.uri }
            }
        };
        let value = action
            .execute(&self.client, &context.cancellation)
            .await
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        // Keep the complete MCP envelope, including URI, MIME, text/blob and metadata.
        Ok(ToolOutput {
            content: Content(vec![ContentBlock::Json {
                value: value.clone(),
            }]),
            structured: Some(value),
            ..Default::default()
        })
    }
}

/// One resource operation. List calls return one server page, including nextCursor.
#[derive(Debug, Clone)]
pub enum McpResourceAction {
    List { cursor: Option<String> },
    Templates { cursor: Option<String> },
    Read { uri: String },
}

impl McpResourceAction {
    async fn execute(
        self,
        client: &McpClient,
        cancellation: &CancellationToken,
    ) -> Result<Value, McpError> {
        match self {
            Self::List { cursor } => {
                serde_json::to_value(client.list_resources(cursor, cancellation).await?)
            }
            Self::Templates { cursor } => {
                serde_json::to_value(client.list_resource_templates(cursor, cancellation).await?)
            }
            Self::Read { uri } => {
                serde_json::to_value(client.read_resource(uri, cancellation).await?)
            }
        }
        .map_err(|_| McpError::Protocol)
    }
}

/// Access resources on an Agent-owned MCP connection with `agent.execute(...)`.
pub struct McpResources {
    pub plugin_id: String,
    pub action: McpResourceAction,
}

#[async_trait]
impl eliteforge_agent_core::AgentOperation for McpResources {
    type Service = McpClientService;
    type Output = Value;

    fn select(&self, services: Vec<Arc<Self::Service>>) -> Result<Arc<Self::Service>, AgentError> {
        services
            .into_iter()
            .find(|service| service.id == self.plugin_id)
            .ok_or_else(|| {
                AgentError::InvalidConfiguration(format!(
                    "MCP plugin not installed: {}",
                    self.plugin_id
                ))
            })
    }

    async fn execute(
        self,
        service: Arc<Self::Service>,
        cancellation: CancellationToken,
    ) -> Result<Value, AgentError> {
        let client = service.client.as_ref().ok_or_else(|| {
            AgentError::InvalidConfiguration("MCP server is not configured".into())
        })?;
        self.action
            .execute(client, &cancellation)
            .await
            .map_err(|e| AgentError::Run(e.to_string()))
    }
}
