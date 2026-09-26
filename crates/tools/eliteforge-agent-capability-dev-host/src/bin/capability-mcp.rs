//! An actual MCP stdio server packaged independently of a sidecar backend.
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt, model::*, service::RequestContext};
use serde_json::json;
use std::sync::Arc;
struct Echo;
impl ServerHandler for Echo {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let result = ListToolsResult {
            tools: vec![Tool::new(
                "echo",
                "Packaged MCP",
                Arc::new(json!({"type":"object"}).as_object().unwrap().clone()),
            )],
            ..Default::default()
        };
        Ok(result)
    }
    async fn call_tool(
        &self,
        input: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        Ok(CallToolResult::structured(json!({"echo":input.arguments})).into())
    }
}
#[tokio::main(worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    std::fs::write("mcp.pid", std::process::id().to_string())?;
    let service = Echo.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
