//! Connects to a real HTTP MCP service only when explicitly run.
use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_storage_sqlite::SqliteStoragePlugin;
use eliteforge_agent_official_plugin_tools_mcp::*;
use std::collections::HashMap;

struct InspectOnly;
#[async_trait]
impl AgentLoop for InspectOnly {
    async fn run(
        &self,
        _: &RunRequest,
        _: &RunContext,
        _: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        Err(AgentError::Run(
            "this example only inspects installed tools".into(),
        ))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::var("MCP_URL")?;
    let mut headers = HashMap::new();
    if let Ok(token) = std::env::var("MCP_TOKEN") {
        headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_str(&format!("Bearer {token}"))?,
        );
    }
    let agent = AgentBuilder::new()
        .plugin(SqliteStoragePlugin::new("agent.sqlite"))
        .plugin(AgentLoopPlugin::new(InspectOnly))
        .plugin(McpToolsPlugin::new(
            "service",
            McpConfig::new("service", McpTransport::StreamableHttp { url, headers }),
        ))
        .build()
        .await?;
    let discovered = agent
        .execute(DiscoverMcpTools {
            plugin_id: "service".into(),
        })
        .await;
    let closed = agent.close().await;
    for tool in discovered? {
        println!("{} -> {}", tool.id, tool.call_name);
    }
    if !closed.is_success() {
        return Err(format!("shutdown failed: {closed:?}").into());
    }
    Ok(())
}
