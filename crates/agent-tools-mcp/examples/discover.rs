//! Connects to a real HTTP MCP service only when explicitly run.
use agent_tools_mcp::*;
use std::collections::HashMap;
use tokio_util::sync::CancellationToken;

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
    let client = McpClient::connect(McpConfig::new(
        "service",
        McpTransport::StreamableHttp { url, headers },
    ))
    .await?;
    let discovered = client.discover(&CancellationToken::new()).await;
    let closed = client.close().await;
    for tool in discovered? {
        println!("{} -> {}", tool.descriptor.id, tool.descriptor.call_name);
    }
    closed?;
    Ok(())
}
