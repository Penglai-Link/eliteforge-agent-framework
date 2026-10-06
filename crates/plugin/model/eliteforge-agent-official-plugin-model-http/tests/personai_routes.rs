use std::sync::Arc;

use crab_serverapi::{AuthorizedClientContext, gateway::QualityType};
use eliteforge_agent_core as fw;
use eliteforge_agent_official_plugin_model_http::*;
use rsa::pkcs8::EncodePublicKey;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn config(protocol: AgentModelProtocol, path: Option<&str>) -> AgentModelConfig {
    AgentModelConfig {
        model_code: "fixture-route".into(),
        protocol,
        base_url: "https://unused.invalid".into(),
        api_path: path.map(str::to_owned),
        api_key: String::new(),
        model_name: String::new(),
        thinking_enabled: false,
        reasoning_effort: None,
        responses_stateful_enabled: false,
        temperature: None,
        max_tokens: None,
        context_window_tokens: None,
        top_p: None,
        max_retries: Some(0),
    }
}

#[tokio::test]
async fn production_protocol_clients_use_personai_api_routes_with_or_without_leading_slash() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/person-ai-index", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        for (path, reply) in [
            (
                "chat/completions",
                json!({"choices":[{"message":{"role":"assistant","content":"OK"},"finish_reason":"stop"}]}),
            ),
            (
                "responses",
                json!({"id":"response-fixture","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"OK"}]}]}),
            ),
            (
                "embeddings",
                json!({"model":"fixture-provider-embedding","data":[{"index":0,"embedding":[0.25,0.75]}]}),
            ),
            (
                "chat/completions",
                json!({"choices":[{"message":{"role":"assistant","content":"OK"},"finish_reason":"stop"}]}),
            ),
        ] {
            let (mut socket, _) =
                tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept())
                    .await
                    .unwrap()
                    .unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0; 4096];
            let (end, length) = loop {
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
                if let Some(end) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|n| n.trim().parse::<usize>().unwrap())
                        })
                        .unwrap();
                    break (end + 4, length);
                }
            };
            while bytes.len() < end + length {
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
            }
            let headers = String::from_utf8_lossy(&bytes[..end]);
            assert_eq!(
                headers.lines().next().unwrap(),
                format!("POST /person-ai-index/api/v1/{path} HTTP/1.1")
            );
            assert!(
                headers
                    .to_ascii_lowercase()
                    .contains("x-product-code: fixture")
            );
            let envelope: Value = serde_json::from_slice(&bytes[end..end + length]).unwrap();
            assert!(envelope.get("messages").is_none());
            assert!(envelope.get("input").is_none());
            let body = reply.to_string();
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
    });
    let key = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048)
        .unwrap()
        .to_public_key()
        .to_public_key_pem(rsa::pkcs8::LineEnding::LF)
        .unwrap();
    let context = AuthorizedClientContext::new()
        .with_token("fixture-token")
        .with_client_id("fixture-client")
        .with_public_key(key)
        .with_product_code("fixture")
        .with_product_version("fixture");
    let transport: Arc<dyn ModelTransport> =
        Arc::new(PersonAiModelTransport::new(url, context, QualityType::High, None).unwrap());
    let client = HttpProtocolClient::new(transport.clone());
    let request = ModelRequest {
        messages: vec![AgentMessage::user("Reply OK")],
        tools: vec![],
        prepared_tool_schema: None,
        system_agent_session_id: None,
        response_input_items: None,
        previous_response_id: None,
    };
    client
        .generate(&config(AgentModelProtocol::ChatCompletions, None), &request)
        .await
        .unwrap();
    client
        .generate(
            &config(AgentModelProtocol::Responses, Some("responses")),
            &request,
        )
        .await
        .unwrap();
    let model = HttpEmbeddingModel {
        config: config(AgentModelProtocol::Embeddings, Some("embeddings")),
        factory: Arc::new(FixedModelTransportFactory(transport)),
    };
    let mut embedding = fw::EmbeddingRequest::texts(["fixture input"]);
    embedding.choice = fw::ModelChoice::model(fw::ModelRef::new("fixture", "embedding"));
    let result = fw::EmbeddingModel::embed(&model, "embedding", embedding)
        .await
        .unwrap();
    assert_eq!(
        result.provider_model.as_deref(),
        Some("fixture-provider-embedding")
    );
    assert_eq!(result.model, fw::ModelRef::new("fixture", "embedding"));
    client
        .generate(
            &config(
                AgentModelProtocol::ChatCompletions,
                Some("/chat/completions"),
            ),
            &request,
        )
        .await
        .unwrap();
    server.await.unwrap();
}
