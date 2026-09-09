//! A harness-free executable also serves as an actual stdio child, avoiding any
//! test-harness text on the MCP protocol stream. All servers use official rmcp.
use eliteforge_agent_framework::{ContentBlock as FrameworkBlock, ToolError};
use eliteforge_agent_official_plugin_tools_mcp::*;
use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    model::*,
    service::RequestContext,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde_json::json;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
struct Server {
    cycle: bool,
    entered: Arc<Notify>,
    cancelled: Arc<Notify>,
}
impl ServerHandler for Server {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }
    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let mut page = ListToolsResult::default();
        let second = request.and_then(|r| r.cursor).is_some();
        let names = if second {
            vec!["slow", "typed.output"]
        } else {
            vec!["echo"]
        };
        page.tools = names
            .into_iter()
            .map(|name| {
                Tool::new(
                    name,
                    "fixture",
                    Arc::new(json!({"type":"object"}).as_object().unwrap().clone()),
                )
            })
            .collect();
        if !second || self.cycle {
            page.next_cursor = Some("page-two".into());
        }
        Ok(page)
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        match request.name.as_ref() {
            "echo" => Ok(CallToolResult::structured(json!({"echo":request.arguments})).into()),
            "typed.output" => {
                let mut result = CallToolResult::error(vec![
                    ContentBlock::text("read failed"),
                    ContentBlock::image("YQ==", "image/png"),
                    ContentBlock::embedded_text("fixture:doc", "source data"),
                ]);
                result.structured_content = Some(json!({"code":"fixture-error"}));
                Ok(result.into())
            }
            "slow" => {
                self.entered.notify_one();
                context.ct.cancelled().await;
                self.cancelled.notify_one();
                Ok(CallToolResult::error(vec![ContentBlock::text("cancelled")]).into())
            }
            _ => Err(ErrorData::invalid_params("unknown tool", None)),
        }
    }
}

async fn http_server(server: Server) -> (String, CancellationToken, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let token = CancellationToken::new();
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let router = axum::Router::new().nest_service("/mcp", service);
    let stop = token.clone();
    let task = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(stop.cancelled_owned())
            .await
            .unwrap();
    });
    (url, token, task)
}

async fn check_roundtrip(client: &McpClient) {
    let cancellation = CancellationToken::new();
    let tools = client.discover(&cancellation).await.unwrap();
    assert_eq!(tools.len(), 3);
    assert!(tools.iter().all(|t| !t.parallel_safe));
    assert!(tools.iter().any(
        |t| t.descriptor.id.ends_with("/typed.output") && !t.descriptor.call_name.contains('.')
    ));
    let result = client
        .call_tool("echo", json!({"value":"真实协议往返"}), &cancellation)
        .await
        .unwrap();
    assert_eq!(result.structured.unwrap()["echo"]["value"], "真实协议往返");
    let result = client
        .call_tool("typed.output", json!({}), &cancellation)
        .await
        .unwrap();
    assert!(result.is_error);
    assert_eq!(result.structured.unwrap()["code"], "fixture-error");
    assert!(
        matches!(&result.content.0[1], FrameworkBlock::Media { media } if media.mime_type == "image/png")
    );
    assert!(
        matches!(&result.content.0[2], FrameworkBlock::Json { value } if value.to_string().contains("source data"))
    );
}

async fn http_roundtrip_and_protocol_cancellation() {
    let server = Server::default();
    let entered = server.entered.clone();
    let cancelled = server.cancelled.clone();
    let (url, stop, task) = http_server(server).await;
    let client = McpClient::connect(McpConfig::new(
        "http",
        McpTransport::StreamableHttp {
            url,
            headers: HashMap::new(),
        },
    ))
    .await
    .unwrap();
    check_roundtrip(&client).await;
    let child = client.clone();
    let token = CancellationToken::new();
    let request_token = token.clone();
    let call =
        tokio::spawn(async move { child.call_tool("slow", json!({}), &request_token).await });
    tokio::time::timeout(Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    token.cancel();
    assert!(matches!(
        call.await.unwrap(),
        Err(ToolError::Indeterminate(_))
    ));
    tokio::time::timeout(Duration::from_secs(3), cancelled.notified())
        .await
        .unwrap();
    // Drop is also a protocol cancellation path, not merely local future removal.
    let child = client.clone();
    let call = tokio::spawn(async move {
        child
            .call_tool("slow", json!({}), &CancellationToken::new())
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    call.abort();
    let _ = call.await;
    tokio::time::timeout(Duration::from_secs(3), cancelled.notified())
        .await
        .unwrap();
    client.close().await.unwrap();
    client.close().await.unwrap();
    assert!(client.is_closed());
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
}

async fn discovery_cycle_is_bounded() {
    let (url, stop, task) = http_server(Server {
        cycle: true,
        ..Default::default()
    })
    .await;
    let client = McpClient::connect(McpConfig::new(
        "bounded",
        McpTransport::StreamableHttp {
            url,
            headers: HashMap::new(),
        },
    ))
    .await
    .unwrap();
    assert!(matches!(
        client.discover(&CancellationToken::new()).await,
        Err(McpError::DiscoveryLimit)
    ));
    client.close().await.unwrap();
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
}

async fn stdio_roundtrip_and_reaped_close() {
    let transport = McpTransport::Stdio {
        program: std::env::current_exe().unwrap(),
        args: vec!["--stdio-fixture".into()],
        env: HashMap::new(),
        current_dir: None,
        inherit_env: false,
    };
    let client = McpClient::connect(McpConfig::new("stdio", transport))
        .await
        .unwrap();
    check_roundtrip(&client).await;
    tokio::time::timeout(Duration::from_secs(5), client.close())
        .await
        .unwrap()
        .unwrap();
    assert!(client.is_closed());
    assert!(matches!(
        client
            .call_tool("echo", json!({}), &CancellationToken::new())
            .await,
        Err(ToolError::Failed(_))
    ));
}

#[tokio::main]
async fn main() {
    if std::env::args().any(|arg| arg == "--stdio-fixture") {
        Server::default()
            .serve(rmcp::transport::stdio())
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap();
        return;
    }
    tokio::time::timeout(Duration::from_secs(30), async {
        http_roundtrip_and_protocol_cancellation().await;
        discovery_cycle_is_bounded().await;
        stdio_roundtrip_and_reaped_close().await;
    })
    .await
    .unwrap();
    println!(
        "MCP transport scenarios: 3 passed (HTTP roundtrip and cancellation, bounded discovery, stdio roundtrip and close)"
    );
}
