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
    resources: bool,
    resources_only: bool,
    entered: Arc<Notify>,
    cancelled: Arc<Notify>,
}
impl ServerHandler for Server {
    fn get_info(&self) -> ServerInfo {
        let mut capabilities = ServerCapabilities::builder().enable_tools().build();
        if self.resources {
            capabilities.resources = Some(ResourcesCapability::default());
        }
        if self.resources_only {
            capabilities.tools = None;
        }
        ServerInfo::new(capabilities)
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
    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let second = request.and_then(|p| p.cursor).is_some();
        let mut page = ListResourcesResult::with_all_items(vec![
            Resource::new(
                if second {
                    "fixture:binary"
                } else {
                    "fixture:text"
                },
                "fixture-resource",
            )
            .with_mime_type(if second {
                "application/octet-stream"
            } else {
                "text/plain"
            }),
        ]);
        if !second || self.cycle {
            page.next_cursor = Some("resource-page-two".into());
        }
        Ok(page)
    }
    async fn list_resource_templates(
        &self,
        request: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        let second = request.and_then(|p| p.cursor).is_some();
        let mut page = ListResourceTemplatesResult::with_all_items(vec![ResourceTemplate::new(
            if second {
                "fixture:binary/{id}"
            } else {
                "fixture:text/{id}"
            },
            "fixture-template",
        )]);
        if !second || self.cycle {
            page.next_cursor = Some("template-page-two".into());
        }
        Ok(page)
    }
    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let item = match request.uri.as_str() {
            "fixture:text" => ResourceContents::text("资源正文", &request.uri).with_meta(
                MetaObject(json!({"origin":"fixture"}).as_object().unwrap().clone()),
            ),
            "fixture:binary" => ResourceContents::blob("AAEC", &request.uri)
                .with_mime_type("application/octet-stream"),
            "fixture:large" => ResourceContents::text("x".repeat(8192), &request.uri),
            "fixture:slow" => {
                self.entered.notify_one();
                context.ct.cancelled().await;
                self.cancelled.notify_one();
                return Err(ErrorData::internal_error("cancelled", None));
            }
            _ => return Err(ErrorData::invalid_params("unknown resource", None)),
        };
        Ok(ReadResourceResult::new(vec![item]).into())
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

struct InspectOnly;
#[async_trait::async_trait]
impl eliteforge_agent_framework::AgentLoop for InspectOnly {
    async fn run(
        &self,
        _: &eliteforge_agent_framework::RunRequest,
        _: &eliteforge_agent_framework::RunContext,
        _: &eliteforge_agent_framework::AgentServices,
    ) -> Result<eliteforge_agent_framework::RunResult, eliteforge_agent_framework::AgentError> {
        unreachable!("discovery must not execute the Agent loop")
    }
}

async fn configured_mcp_plugins_are_discovered_and_closed_through_agent() {
    use eliteforge_agent_framework::{AgentBuilder, AgentError, AgentLoopPlugin};
    use eliteforge_agent_official_plugin_storage_sqlite::SqliteStoragePlugin;
    let dir = tempfile::tempdir().unwrap();
    let (url, stop, task) = http_server(Server::default()).await;
    let config = |namespace| {
        McpConfig::new(
            namespace,
            McpTransport::StreamableHttp {
                url: url.clone(),
                headers: HashMap::new(),
            },
        )
    };
    let agent = AgentBuilder::new()
        .plugin(SqliteStoragePlugin::new(dir.path().join("mcp.db")))
        .plugin(AgentLoopPlugin::new(InspectOnly))
        .plugin(McpToolsPlugin::new("first", config("first")))
        .plugin(McpToolsPlugin::new("second", config("second")))
        .build()
        .await
        .unwrap();
    for id in ["first", "second"] {
        let tools = agent
            .execute(DiscoverMcpTools {
                plugin_id: id.into(),
            })
            .await
            .unwrap();
        assert_eq!(tools.len(), 3);
        assert!(
            tools
                .iter()
                .all(|tool| tool.id.starts_with(&format!("mcp/{id}/")))
        );
    }
    assert!(
        agent
            .execute(DiscoverMcpTools {
                plugin_id: "missing".into()
            })
            .await
            .is_err()
    );
    assert!(agent.close().await.is_success());
    assert!(matches!(
        agent
            .execute(DiscoverMcpTools {
                plugin_id: "first".into()
            })
            .await,
        Err(AgentError::Closed)
    ));
    stop.cancel();
    task.await.unwrap();
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

async fn resource_roundtrip(client: &McpClient) {
    let cancellation = CancellationToken::new();
    let first = client.list_resources(None, &cancellation).await.unwrap();
    assert_eq!(first.resources[0].uri, "fixture:text");
    let second = client
        .list_resources(first.next_cursor, &cancellation)
        .await
        .unwrap();
    assert_eq!(second.resources[0].uri, "fixture:binary");
    assert_eq!(second.next_cursor, None);
    let first = client
        .list_resource_templates(None, &cancellation)
        .await
        .unwrap();
    assert_eq!(
        first.resource_templates[0].uri_template,
        "fixture:text/{id}"
    );
    let second = client
        .list_resource_templates(first.next_cursor, &cancellation)
        .await
        .unwrap();
    assert_eq!(
        second.resource_templates[0].uri_template,
        "fixture:binary/{id}"
    );
    let text = serde_json::to_value(
        client
            .read_resource("fixture:text".into(), &cancellation)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(text["contents"][0]["text"], "资源正文");
    assert_eq!(text["contents"][0]["_meta"]["origin"], "fixture");
    let blob = serde_json::to_value(
        client
            .read_resource("fixture:binary".into(), &cancellation)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(blob["contents"][0]["blob"], "AAEC");
    assert_eq!(blob["contents"][0]["mimeType"], "application/octet-stream");
    assert_eq!(blob["contents"][0]["uri"], "fixture:binary");
    assert!(
        client
            .read_resource("unknown".into(), &cancellation)
            .await
            .is_err()
    );
}

async fn resources_over_http_and_stdio_and_bounded_cursors() {
    let (url, stop, task) = http_server(Server {
        resources: true,
        ..Default::default()
    })
    .await;
    let client = McpClient::connect(McpConfig::new(
        "resources",
        McpTransport::StreamableHttp {
            url,
            headers: HashMap::new(),
        },
    ))
    .await
    .unwrap();
    resource_roundtrip(&client).await;
    let bindings = client.discover(&CancellationToken::new()).await.unwrap();
    assert_eq!(bindings.len(), 6);
    assert!(
        bindings
            .iter()
            .any(|b| b.descriptor.call_name == "mcp_resources__resources__read")
    );
    client.close().await.unwrap();
    stop.cancel();
    task.await.unwrap();
    let client = McpClient::connect(McpConfig::new(
        "stdio-resources",
        McpTransport::Stdio {
            program: std::env::current_exe().unwrap(),
            args: vec!["--stdio-resource-fixture".into()],
            env: HashMap::new(),
            current_dir: None,
            inherit_env: false,
        },
    ))
    .await
    .unwrap();
    resource_roundtrip(&client).await;
    client.close().await.unwrap();
    let mut config = McpConfig::new(
        "stdio-limits",
        McpTransport::Stdio {
            program: std::env::current_exe().unwrap(),
            args: vec!["--stdio-resource-fixture".into()],
            env: HashMap::new(),
            current_dir: None,
            inherit_env: false,
        },
    );
    config.max_result_bytes = 1024;
    let client = McpClient::connect(config).await.unwrap();
    assert!(matches!(
        client
            .read_resource("fixture:large".into(), &CancellationToken::new())
            .await,
        Err(McpError::ResultLimit)
    ));
    client.close().await.unwrap();
    let (url, stop, task) = http_server(Server {
        resources: true,
        resources_only: true,
        cycle: true,
        ..Default::default()
    })
    .await;
    let mut config = McpConfig::new(
        "bounded-resources",
        McpTransport::StreamableHttp {
            url,
            headers: HashMap::new(),
        },
    );
    config.max_result_bytes = 1024;
    let client = McpClient::connect(config).await.unwrap();
    assert_eq!(
        client
            .discover(&CancellationToken::new())
            .await
            .unwrap()
            .len(),
        3
    );
    let cancellation = CancellationToken::new();
    assert!(matches!(
        client
            .list_resources(Some("resource-page-two".into()), &cancellation)
            .await,
        Err(McpError::DiscoveryLimit)
    ));
    assert!(matches!(
        client
            .list_resource_templates(Some("template-page-two".into()), &cancellation)
            .await,
        Err(McpError::DiscoveryLimit)
    ));
    // HTTP SSE rejects the oversized frame before resource decoding. The SDK
    // reports a transport request failure; stdio exercises the decoded-size guard.
    assert!(
        client
            .read_resource("fixture:text".into(), &cancellation)
            .await
            .is_ok()
    );
    let oversized = client
        .read_resource("fixture:large".into(), &cancellation)
        .await;
    assert!(
        matches!(oversized, Err(McpError::ResultLimit | McpError::Request)),
        "{oversized:?}"
    );
    client.close().await.unwrap();
    assert!(matches!(
        client.discover(&cancellation).await,
        Err(McpError::Closed)
    ));
    stop.cancel();
    task.await.unwrap();
}

async fn resource_reads_propagate_cancel_and_timeout() {
    for timeout in [false, true] {
        let server = Server {
            resources: true,
            ..Default::default()
        };
        let entered = server.entered.clone();
        let cancelled = server.cancelled.clone();
        let (url, stop, task) = http_server(server).await;
        let mut config = McpConfig::new(
            "cancel-resources",
            McpTransport::StreamableHttp {
                url,
                headers: HashMap::new(),
            },
        );
        config.request_timeout = if timeout {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(5)
        };
        let client = McpClient::connect(config).await.unwrap();
        let cancellation = CancellationToken::new();
        let request_cancel = cancellation.clone();
        let request_client = client.clone();
        let request = tokio::spawn(async move {
            request_client
                .read_resource("fixture:slow".into(), &request_cancel)
                .await
        });
        entered.notified().await;
        if !timeout {
            cancellation.cancel();
        }
        let result = request.await.unwrap();
        assert!(if timeout {
            matches!(result, Err(McpError::Timeout))
        } else {
            matches!(result, Err(McpError::Cancelled))
        });
        tokio::time::timeout(Duration::from_secs(2), cancelled.notified())
            .await
            .unwrap();
        client.close().await.unwrap();
        stop.cancel();
        task.await.unwrap();
    }
}

struct ReadResourceLoop;
#[async_trait::async_trait]
impl eliteforge_agent_framework::AgentLoop for ReadResourceLoop {
    async fn run(
        &self,
        _: &eliteforge_agent_framework::RunRequest,
        context: &eliteforge_agent_framework::RunContext,
        services: &eliteforge_agent_framework::AgentServices,
    ) -> Result<eliteforge_agent_framework::RunResult, eliteforge_agent_framework::AgentError> {
        for provider in &services.tools {
            let lease = provider.lease(context).await?;
            if let Some(binding) = lease
                .tools
                .iter()
                .find(|b| b.descriptor.call_name == "mcp_resources__agent__read")
            {
                let output = binding
                    .executor
                    .call(json!({"uri":"fixture:text"}), context)
                    .await
                    .unwrap();
                assert_eq!(
                    output.structured.as_ref().unwrap()["contents"][0]["text"],
                    "资源正文"
                );
                return Ok(eliteforge_agent_framework::RunResult {
                    output: String::new(),
                    content: output.content,
                    structured: output.structured,
                    status: eliteforge_agent_framework::RunStatus::Completed,
                    interactions: Vec::new(),
                    usage: Default::default(),
                    steps: 0,
                    tool_calls: 1,
                });
            }
        }
        panic!("MCP resource tool must be visible to the Agent");
    }
}

async fn resource_operations_and_tools_are_agent_owned() {
    use eliteforge_agent_framework::{AgentBuilder, AgentLoopPlugin, RunRequest};
    use eliteforge_agent_official_plugin_storage_sqlite::SqliteStoragePlugin;
    let dir = tempfile::tempdir().unwrap();
    let (url, stop, task) = http_server(Server {
        resources: true,
        resources_only: true,
        ..Default::default()
    })
    .await;
    let agent = AgentBuilder::new()
        .default_models(false)
        .plugin(SqliteStoragePlugin::new(dir.path().join("resources.db")))
        .plugin(AgentLoopPlugin::new(ReadResourceLoop))
        .plugin(McpToolsPlugin::new(
            "resources",
            McpConfig::new(
                "agent",
                McpTransport::StreamableHttp {
                    url,
                    headers: HashMap::new(),
                },
            ),
        ))
        .build()
        .await
        .unwrap();
    let listed = agent
        .execute(McpResources {
            plugin_id: "resources".into(),
            action: McpResourceAction::List { cursor: None },
        })
        .await
        .unwrap();
    assert_eq!(listed["resources"][0]["uri"], "fixture:text");
    let templates = agent
        .execute(McpResources {
            plugin_id: "resources".into(),
            action: McpResourceAction::Templates { cursor: None },
        })
        .await
        .unwrap();
    assert_eq!(
        templates["resourceTemplates"][0]["uriTemplate"],
        "fixture:text/{id}"
    );
    let read = agent
        .execute(McpResources {
            plugin_id: "resources".into(),
            action: McpResourceAction::Read {
                uri: "fixture:binary".into(),
            },
        })
        .await
        .unwrap();
    assert_eq!(read["contents"][0]["blob"], "AAEC");
    agent
        .run(RunRequest::new("resource-session", "read resource"))
        .await
        .unwrap();
    assert!(agent.close().await.is_success());
    stop.cancel();
    task.await.unwrap();
}

fn content_length_fixture() {
    use std::io::{BufRead, Read, Write};
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    loop {
        let mut line = String::new();
        if input.read_line(&mut line).unwrap() == 0 {
            return;
        }
        let Some(length) = line
            .strip_prefix("Content-Length:")
            .and_then(|v| v.trim().parse::<usize>().ok())
        else {
            return;
        };
        line.clear();
        input.read_line(&mut line).unwrap();
        let mut bytes = vec![0; length];
        input.read_exact(&mut bytes).unwrap();
        let request: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let Some(id) = request.get("id") else {
            continue;
        };
        let result = match request["method"].as_str().unwrap() {
            "initialize" => {
                json!({"protocolVersion":request["params"]["protocolVersion"],"serverInfo":{"name":"legacy-fixture","version":"1"},"capabilities":{"tools":{}}})
            }
            "tools/list" => {
                json!({"tools":[{"name":"echo","description":"echo","inputSchema":{"type":"object"}}]})
            }
            "tools/call" => {
                json!({"content":[{"type":"text","text":request["params"]["arguments"].to_string()}]})
            }
            "ping" => json!({}),
            _ => return,
        };
        let response =
            serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"result":result})).unwrap();
        write!(output, "Content-Length: {}\r\n\r\n", response.len()).unwrap();
        output.write_all(&response).unwrap();
        output.flush().unwrap();
    }
}
async fn legacy_content_length_uses_sdk_discovery_and_calls() {
    for framing in [McpStdioFraming::ContentLength, McpStdioFraming::Auto] {
        let mut config = McpConfig::new(
            "legacy",
            McpTransport::Stdio {
                program: std::env::current_exe().unwrap(),
                args: vec!["--content-length-fixture".into()],
                env: HashMap::new(),
                current_dir: None,
                inherit_env: false,
            },
        );
        config.stdio_framing = framing;
        config.connect_timeout = Duration::from_millis(500);
        let client = McpClient::connect(config).await.unwrap();
        let bindings = client.discover(&CancellationToken::new()).await.unwrap();
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].descriptor.id, "mcp/legacy/echo");
        let output = client
            .call_tool("echo", json!({"value":"中文"}), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(output.content.text(), r#"{"value":"中文"}"#);
        client.close().await.unwrap();
        assert!(client.is_closed());
    }
}

#[tokio::main]
async fn main() {
    if std::env::args().any(|arg| arg == "--content-length-fixture") {
        content_length_fixture();
        return;
    }
    if std::env::args().any(|arg| arg == "--stdio-fixture" || arg == "--stdio-resource-fixture") {
        Server {
            resources: std::env::args().any(|arg| arg == "--stdio-resource-fixture"),
            ..Default::default()
        }
        .serve(rmcp::transport::stdio())
        .await
        .unwrap()
        .waiting()
        .await
        .unwrap();
        return;
    }
    tokio::time::timeout(Duration::from_secs(30), async {
        legacy_content_length_uses_sdk_discovery_and_calls().await;
        http_roundtrip_and_protocol_cancellation().await;
        discovery_cycle_is_bounded().await;
        stdio_roundtrip_and_reaped_close().await;
        configured_mcp_plugins_are_discovered_and_closed_through_agent().await;
        resources_over_http_and_stdio_and_bounded_cursors().await;
        resource_reads_propagate_cancel_and_timeout().await;
        resource_operations_and_tools_are_agent_owned().await;
    })
    .await
    .unwrap();
    println!(
        "MCP transport scenarios: 8 passed (tools, resources, HTTP/stdio, limits, cancellation, Agent operations and close)"
    );
}
