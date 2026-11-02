//! Actual loopback requests: no credentials or external services are needed.
use eliteforge_agent_official_plugin_model_http::*;
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

type Captured = Arc<Mutex<Vec<(String, Value)>>>;
struct Server {
    url: String,
    captured: Captured,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn server(responses: Vec<(&'static str, String)>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/gateway/v1", listener.local_addr().unwrap());
    let captured: Captured = Arc::default();
    let saved = captured.clone();
    let task = tokio::spawn(async move {
        for (content_type, body) in responses {
            let (mut socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0; 4096];
            let (header, offset) = loop {
                let n = socket.read(&mut chunk).await.unwrap();
                assert_ne!(n, 0);
                bytes.extend_from_slice(&chunk[..n]);
                if let Some(end) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                    break (String::from_utf8(bytes[..end].to_vec()).unwrap(), end + 4);
                }
            };
            let length = header
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|n| n.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            while bytes.len() < offset + length {
                let n = socket.read(&mut chunk).await.unwrap();
                assert_ne!(n, 0);
                bytes.extend_from_slice(&chunk[..n]);
            }
            saved.lock().unwrap().push((
                header,
                serde_json::from_slice(&bytes[offset..offset + length]).unwrap(),
            ));
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            for chunk in body.as_bytes().chunks(17) {
                socket.write_all(chunk).await.unwrap();
            }
        }
    });
    Server {
        url,
        captured,
        task,
    }
}
fn config(protocol: AgentModelProtocol, url: &str) -> AgentModelConfig {
    serde_json::from_value(json!({
        "modelCode":"custom-connection/model", "protocol":protocol, "baseUrl":url,
        "apiPath":null, "apiKey":"fixture-key", "modelName":"custom-model",
        "requestHeaders":{"x-tenant":"fixture-tenant"}, "maxTokens":128,
        "contextWindowTokens":16384
    }))
    .unwrap()
}
fn client(config: &AgentModelConfig) -> HttpProtocolClient {
    HttpProtocolClient::new(Arc::new(
        HttpModelTransport::new(ModelHttpClient::new(), &config.base_url, &config.api_key)
            .with_headers(&config.request_headers)
            .unwrap(),
    ))
}

#[tokio::test]
async fn native_model_ids_with_reasoning_suffixes_require_explicit_effort() {
    for protocol in [AgentModelProtocol::Anthropic, AgentModelProtocol::Gemini] {
        for suffix in [
            "none", "zero", "minimal", "low", "medium", "high", "xhigh", "max",
        ] {
            let mut config = config(protocol, "http://127.0.0.1:1/v1");
            config.thinking_enabled = true;
            config.model_name = format!("custom-model-{suffix}");
            let error = client(&config)
                .generate(&config, &request())
                .await
                .unwrap_err();
            assert!(
                matches!(error, eliteforge_agent_core::ModelError::Fatal(ref message)
                if message.contains("explicit reasoning effort"))
            );
        }
    }
}
fn request() -> ModelRequest {
    ModelRequest {
        messages: vec![
            AgentMessage::system("Be brief."),
            AgentMessage::user("Hello"),
        ],
        tools: vec![EffectiveTool {
            tool_name: "lookup".into(),
            description: "Look up".into(),
            input_schema: json!({"type":"object","properties":{"key":{"type":"string"}}}),
        }],
        prepared_tool_schema: None,
        system_agent_session_id: None,
        response_input_items: None,
        previous_response_id: None,
    }
}
fn anthropic(events: &[(&str, Value)]) -> (&'static str, String) {
    (
        "text/event-stream",
        events
            .iter()
            .map(|(name, body)| format!("event: {name}\ndata: {body}\n\n"))
            .collect(),
    )
}
fn anthropic_text(text: &str, with_usage: bool) -> (&'static str, String) {
    let mut start = json!({"type":"message_start","message":{"id":"m2","role":"assistant","model":"custom-model","content":[]}});
    let mut end = json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}});
    if with_usage {
        start["message"]["usage"] = json!({"input_tokens":20,"output_tokens":0});
        end["usage"] = json!({"output_tokens":2});
    }
    anthropic(&[
        ("message_start", start),
        (
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        ),
        (
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
        ),
        (
            "content_block_stop",
            json!({"type":"content_block_stop","index":0}),
        ),
        ("message_delta", end),
        ("message_stop", json!({"type":"message_stop"})),
    ])
}

#[tokio::test]
async fn custom_openai_protocols_preserve_path_headers_model_and_usage() {
    for (protocol, path, body) in [
        (
            AgentModelProtocol::ChatCompletions,
            "chat/completions",
            json!({"choices":[{"message":{"role":"assistant","content":"OK"},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":2,"total_tokens":9}}),
        ),
        (
            AgentModelProtocol::Responses,
            "responses",
            json!({"id":"r1","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"OK"}]}],"usage":{"input_tokens":7,"output_tokens":2,"total_tokens":9}}),
        ),
    ] {
        let fixture = server(vec![("application/json", body.to_string())]).await;
        let config = config(protocol, &fixture.url);
        let response = client(&config).generate(&config, &request()).await.unwrap();
        assert_eq!(response.output_text(), "OK");
        assert_eq!(response.token_usage.unwrap().total_tokens, 9);
        let calls = fixture.captured.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].0.starts_with(&format!("POST /gateway/v1/{path} ")));
        let header = calls[0].0.to_ascii_lowercase();
        assert!(header.contains("authorization: bearer fixture-key"));
        assert!(header.contains("x-tenant: fixture-tenant"));
        assert_eq!(calls[0].1["model"], "custom-model");
        assert!(!calls[0].1["tools"].as_array().unwrap().is_empty());
    }
}

#[tokio::test]
async fn anthropic_stream_round_trips_signed_thinking_and_tool_results() {
    let fixture=server(vec![anthropic(&[
        ("message_start",json!({"type":"message_start","message":{"id":"m1","usage":{"input_tokens":5,"output_tokens":0,"cache_read_input_tokens":3}}})),
        ("content_block_start",json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}})),
        ("content_block_delta",json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"reason"}})),
        ("content_block_delta",json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"signed"}})),
        ("content_block_stop",json!({"type":"content_block_stop","index":0})),
        ("content_block_start",json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"c1","name":"lookup","input":{}}})),
        ("content_block_delta",json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"key\":\"value\"}"}})),
        ("content_block_stop",json!({"type":"content_block_stop","index":1})),
        ("message_delta",json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":10}})),
        ("message_stop",json!({"type":"message_stop"})),
    ]),anthropic_text("complete",true)]).await;
    let config = config(AgentModelProtocol::Anthropic, &fixture.url);
    let client = client(&config);
    let request = request();
    let response = client
        .generate_stream(&config, &request, Arc::new(|_| {}))
        .await
        .unwrap();
    assert_eq!(response.finish, ModelFinish::ToolCalls);
    assert_eq!(response.tool_calls()[0].call_id.as_deref(), Some("c1"));
    let usage = response.token_usage.unwrap();
    assert_eq!(usage.output_tokens, 10);
    assert_eq!(usage.cached_input_tokens, Some(3));
    let native = response_to_framework(response).unwrap();
    let mut admitted = request_to_framework(request).unwrap();
    let mut message = eliteforge_agent_core::Message::assistant(native.content);
    message.tool_calls = native.tool_calls;
    message.model_context = native.model_context;
    admitted.messages.push(message);
    let mut request = request_from_framework(&admitted).unwrap();
    request.messages.push(AgentMessage::tool_result(
        "lookup",
        Some("c1".into()),
        "found",
    ));
    let streamed = Arc::new(Mutex::new(String::new()));
    let seen = streamed.clone();
    let response = client
        .generate_stream(
            &config,
            &request,
            Arc::new(move |event| {
                if event.kind == ModelStreamEventKind::Text {
                    seen.lock().unwrap().push_str(&event.delta)
                }
            }),
        )
        .await
        .unwrap();
    assert_eq!(response.output_text(), "complete");
    assert_eq!(*streamed.lock().unwrap(), "complete");
    let calls = fixture.captured.lock().unwrap();
    assert_eq!(calls.len(), 2);
    let headers = calls[0].0.to_ascii_lowercase();
    assert!(headers.starts_with("post /gateway/v1/messages "));
    assert!(headers.contains("x-api-key: fixture-key"));
    assert!(headers.contains("x-tenant: fixture-tenant"));
    assert!(!headers.contains("authorization: bearer"));
    assert_eq!(calls[0].1["model"], "custom-model");
    assert_eq!(calls[0].1["tools"][0]["name"], "lookup");
    let messages = calls[1].1["messages"].as_array().unwrap();
    let assistant = messages.iter().find(|m| m["role"] == "assistant").unwrap();
    assert_eq!(
        assistant["content"][0],
        json!({"type":"thinking","thinking":"reason","signature":"signed"})
    );
    assert_eq!(assistant["content"].as_array().unwrap().len(), 2);
    assert_eq!(assistant["content"][1]["id"], "c1");
    assert!(
        !calls[1]
            .1
            .to_string()
            .contains("framework_genai_continuation_v1")
    );
}

#[tokio::test]
async fn gemini_uses_explicit_protocol_url_auth_media_and_usage() {
    let event = json!({"candidates":[{"content":{"role":"model","parts":[{"text":"seen"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":11,"candidatesTokenCount":2,"thoughtsTokenCount":3,"cachedContentTokenCount":4}});
    let fixture = server(vec![("text/event-stream", format!("data: {event}\n\n"))]).await;
    let config = config(AgentModelProtocol::Gemini, &fixture.url);
    let mut request = request();
    request.messages[1].parts.push(AgentMessagePart::Image {
        mime_type: "image/png".into(),
        data: "YQ==".into(),
    });
    let response = client(&config).generate(&config, &request).await.unwrap();
    assert_eq!(response.output_text(), "seen");
    let usage = response.token_usage.unwrap();
    assert_eq!(usage.input_tokens, 11);
    assert_eq!(usage.output_tokens, 5);
    assert_eq!(usage.reasoning_tokens, Some(3));
    let calls = fixture.captured.lock().unwrap();
    assert_eq!(calls.len(), 1);
    let header = calls[0].0.to_ascii_lowercase();
    assert!(header.starts_with("post /gateway/v1/models/custom-model:streamgeneratecontent"));
    assert!(header.contains("x-goog-api-key: fixture-key"));
    assert!(header.contains("x-tenant: fixture-tenant"));
    assert!(!header.contains("authorization: bearer"));
    let body = calls[0].1.to_string();
    assert!(body.contains("image/png"));
    assert!(body.contains("YQ=="));
    assert!(body.contains("lookup"));
}

#[tokio::test]
async fn local_openai_without_a_key_sends_no_authorization() {
    let fixture = server(vec![("application/json", json!({"choices":[{"message":{"role":"assistant","content":"local"},"finish_reason":"stop"}]}).to_string())]).await;
    let mut config = config(AgentModelProtocol::ChatCompletions, &fixture.url);
    config.api_key.clear();
    assert_eq!(
        client(&config)
            .generate(&config, &request())
            .await
            .unwrap()
            .output_text(),
        "local"
    );
    let calls = fixture.captured.lock().unwrap();
    assert!(!calls[0].0.to_ascii_lowercase().contains("authorization:"));
    for name in ["Authorization", "x-api-key", "X-Goog-Api-Key", "Cookie"] {
        assert!(
            validate_request_headers(&[(name.into(), "private".into())].into_iter().collect())
                .is_err()
        );
    }
}

#[tokio::test]
async fn distinct_connections_do_not_share_signed_continuation() {
    let fixture = server(vec![
        anthropic_text("first", true),
        anthropic_text("second", true),
    ])
    .await;
    let first = config(AgentModelProtocol::Anthropic, &fixture.url);
    let response = client(&first).generate(&first, &request()).await.unwrap();
    let mut continuation = response.assistant;
    // A different connection must not decode even deliberately invalid opaque
    // data from the first connection; plain public conversation remains valid.
    for part in &mut continuation.parts {
        if let AgentMessagePart::Context {
            kind,
            metadata: Some(metadata),
            ..
        } = part
        {
            if kind == "framework_genai_continuation_v1" {
                metadata["data"] = json!({"notNative": true});
            }
        }
    }
    let mut second = first.clone();
    second.model_code = "other-connection/model".into();
    second
        .request_headers
        .insert("x-tenant".into(), "other-tenant".into());
    let mut next = request();
    next.messages.push(continuation);
    next.messages.push(AgentMessage::user("Continue"));
    assert_eq!(
        client(&second)
            .generate(&second, &next)
            .await
            .unwrap()
            .output_text(),
        "second"
    );
    let calls = fixture.captured.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls[0].0.contains("fixture-tenant"));
    assert!(calls[1].0.contains("other-tenant"));
    assert!(!calls[1].1.to_string().contains("notNative"));
}

#[tokio::test]
async fn missing_usage_stays_unknown_and_invalid_headers_never_reach_network() {
    let fixture = server(vec![anthropic_text("done", false)]).await;
    let config = config(AgentModelProtocol::Anthropic, &fixture.url);
    let response = client(&config).generate(&config, &request()).await.unwrap();
    assert_eq!(response.token_usage, None);
    for (name, value) in [
        ("x-test", "secret\r\nInjected: value"),
        ("Host", "other.test"),
        ("bad name", "secret"),
    ] {
        let headers = [(name.into(), value.into())].into_iter().collect();
        let error = validate_request_headers(&headers).unwrap_err().to_string();
        assert!(!error.contains("secret"));
    }
    assert_eq!(fixture.captured.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn text_rerank_adapter_rejects_media_before_transport_instead_of_discarding_it() {
    use eliteforge_agent_core as fw;
    use fw::RerankModel;
    struct NoTransport;
    #[async_trait::async_trait]
    impl ModelTransportFactory for NoTransport {
        async fn create(
            &self,
            _: &AgentModelConfig,
        ) -> Result<Arc<dyn ModelTransport>, fw::ModelError> {
            panic!("unsupported media must be rejected before creating transport")
        }
    }
    let model = HttpRerankModel {
        config: config(AgentModelProtocol::ChatCompletions, "http://127.0.0.1:1"),
        factory: Arc::new(NoTransport),
        chat_fallback: false,
        chat_instruction: None,
    };
    let mut image = fw::Content::from("caption");
    image.0.push(fw::ContentBlock::Media {
        media: fw::MediaContent {
            mime_type: "image/png".into(),
            source: fw::MediaSource::Base64("AA==".into()),
            name: None,
            estimated_tokens: Some(10),
        },
    });
    let error = model
        .rerank(
            "test",
            fw::RerankRequest {
                choice: Default::default(),
                query: "query".into(),
                documents: vec![image],
                top_n: Some(1),
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error,fw::ModelError::Fatal(message) if message.contains("media-capable adapter"))
    );
}
