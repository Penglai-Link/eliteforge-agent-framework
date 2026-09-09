//! Real HTTP/SSE exercised against loopback fixtures; no external keys or services.
use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_model_genai::*;
use eliteforge_agent_test_support::*;
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

#[tokio::test]
async fn openai_embeddings_use_embedding_endpoint_even_with_responses_chat_protocol() {
    let fixture = server(vec![(
        200,
        "application/json",
        json!({"model":"vectors","data":[
        {"index":1,"embedding":[0.3,0.4]}, {"index":0,"embedding":[0.1,0.2]}
    ],"usage":{"prompt_tokens":5,"total_tokens":5}})
        .to_string(),
    )])
    .await;
    let agent = TestBuilder::default()
        .plugin(
            GenaiPlugin::new("chat")
                .adapter(AdapterKind::OpenAIResp)
                .endpoint(&fixture.url)
                .auth(AuthData::from_single("fixture-key"))
                .embedding_model("vectors"),
        )
        .build()
        .await
        .unwrap();
    let mut request = EmbeddingRequest::texts(["one", "two"]);
    request.dimensions = Some(2);
    let response = agent.models().embed(request).await.unwrap();
    assert_eq!(response.model, ModelRef::new("default", "vectors"));
    assert_eq!(response.input_tokens, Some(5));
    assert_eq!(response.vectors[0].index, 0);
    assert_eq!(response.vectors[0].values, vec![0.1, 0.2]);
    let captured = fixture.captured.lock().unwrap().clone();
    assert!(captured[0].0.starts_with("POST /v1/embeddings "));
    assert_eq!(captured[0].1["input"], json!(["one", "two"]));
    assert_eq!(captured[0].1["dimensions"], 2);
    assert!(captured[0].1.get("messages").is_none());
    agent.close().await;
}

#[tokio::test]
async fn embedding_task_types_and_missing_usage_are_preserved() {
    for (kind, body) in [
        (
            AdapterKind::Gemini,
            json!({"embeddings":[{"values":[0.1,0.2]}]}),
        ),
        (
            AdapterKind::Cohere,
            json!({"embeddings":{"float":[[0.1,0.2]]}}),
        ),
    ] {
        let fixture = server(vec![(200, "application/json", body.to_string())]).await;
        let agent = TestBuilder::default()
            .plugin(
                GenaiPlugin::new("chat")
                    .adapter(kind)
                    .endpoint(&fixture.url)
                    .auth(AuthData::from_single("fixture-key"))
                    .embedding_model("vectors"),
            )
            .build()
            .await
            .unwrap();
        let mut request = EmbeddingRequest::texts(["search"]);
        request.purpose = EmbeddingPurpose::Query;
        request.dimensions = Some(2);
        let response = agent.models().embed(request).await.unwrap();
        assert_eq!(response.input_tokens, None);
        let body = fixture.captured.lock().unwrap()[0].1.clone();
        if kind == AdapterKind::Gemini {
            assert_eq!(body["requests"][0]["taskType"], "RETRIEVAL_QUERY");
            assert_eq!(body["requests"][0]["outputDimensionality"], 2);
        } else {
            assert_eq!(body["input_type"], "search_query");
            assert_eq!(body["output_dimension"], 2);
            assert_eq!(body["truncate"], "NONE");
        }
        agent.close().await;
    }
}

#[tokio::test]
async fn invalid_embeddings_and_unsupported_capabilities_do_not_succeed() {
    let fixture = server(vec![(
        200,
        "application/json",
        json!({"model":"vectors","data":[
        {"index":0,"embedding":[0.1,0.2]}, {"index":0,"embedding":[0.3,0.4]}
    ],"usage":{"prompt_tokens":5,"total_tokens":5}})
        .to_string(),
    )])
    .await;
    let agent = TestBuilder::default()
        .plugin(
            GenaiPlugin::new("chat")
                .endpoint(&fixture.url)
                .auth(AuthData::from_single("fixture-key"))
                .embedding_model("vectors"),
        )
        .build()
        .await
        .unwrap();
    assert!(
        agent
            .models()
            .embed(EmbeddingRequest::texts(["one", "two"]))
            .await
            .is_err()
    );
    let mut request = EmbeddingRequest::texts(["one"]);
    request.inputs = vec![Content(vec![ContentBlock::Media {
        media: MediaContent {
            mime_type: "image/png".into(),
            source: MediaSource::Base64("YQ==".into()),
            name: None,
            estimated_tokens: Some(1),
        },
    }])];
    assert!(agent.models().embed(request).await.is_err());
    assert_eq!(fixture.captured.lock().unwrap().len(), 1);
    assert!(
        agent
            .models()
            .models(ModelOperation::ImageGeneration)
            .is_empty()
    );
    agent.close().await;
    assert!(
        TestBuilder::default()
            .plugin(
                GenaiPlugin::new("chat")
                    .adapter(AdapterKind::Anthropic)
                    .embedding_model("unavailable")
            )
            .build()
            .await
            .is_err()
    );
}

#[tokio::test]
async fn one_connection_binds_chat_models_to_different_wire_protocols() {
    let response = sse(vec![
        json!({"type":"response.output_text.delta","delta":"responses answer"}),
        json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","model":"responses-model","output":[]}}),
    ]);
    let fixture = server(vec![text_sse("chat answer"), response]).await;
    let agent = TestBuilder::default()
        .plugin(
            GenaiPlugin::new("chat-model")
                .endpoint(&fixture.url)
                .auth(AuthData::from_single("fixture-key"))
                .chat_model_with_protocol(
                    ModelDescriptor::new("responses-model"),
                    OpenAiChatProtocol::Responses,
                ),
        )
        .build()
        .await
        .unwrap();
    for (model, answer) in [
        ("chat-model", "chat answer"),
        ("responses-model", "responses answer"),
    ] {
        let mut request = RunRequest::new(model, "hello");
        request.model = Some(ModelRef::new("default", model));
        assert_eq!(agent.run(request).await.unwrap().output, answer);
    }
    let captured = fixture.captured.lock().unwrap().clone();
    assert!(captured[0].0.starts_with("POST /v1/chat/completions "));
    assert!(captured[1].0.starts_with("POST /v1/responses "));
    assert!(captured[0].1.get("messages").is_some());
    assert!(captured[1].1.get("input").is_some());
    agent.close().await;
}

#[tokio::test]
async fn gemini_chat_keeps_native_audio_and_video_parts() {
    let fixture = server(vec![sse(vec![json!({"candidates":[{"content":{"role":"model","parts":[{"text":"understood"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":2}})])]).await;
    let agent = TestBuilder::default()
        .plugin(
            GenaiPlugin::empty()
                .adapter(AdapterKind::Gemini)
                .endpoint(&fixture.url)
                .auth(AuthData::from_single("fixture-key"))
                .chat_model(ModelDescriptor::new("multimodal").with_input_modalities([
                    Modality::Text,
                    Modality::Audio,
                    Modality::Video,
                ])),
        )
        .build()
        .await
        .unwrap();
    let parts = ["audio/wav", "video/mp4"]
        .into_iter()
        .map(|mime| ContentBlock::Media {
            media: MediaContent {
                mime_type: mime.into(),
                source: MediaSource::Base64("YQ==".into()),
                name: None,
                estimated_tokens: Some(5),
            },
        })
        .collect();
    assert_eq!(
        agent
            .run(RunRequest::new("media", Content(parts)))
            .await
            .unwrap()
            .output,
        "understood"
    );
    let body = fixture.captured.lock().unwrap()[0].1.clone();
    let encoded = body["contents"][0]["parts"].to_string();
    assert!(encoded.contains("audio/wav"));
    assert!(encoded.contains("video/mp4"));
    agent.close().await;
}
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
async fn server(responses: Vec<(u16, &'static str, String)>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/", listener.local_addr().unwrap());
    let captured: Captured = Arc::default();
    let saved = captured.clone();
    let task = tokio::spawn(async move {
        for (status, content_type, body) in responses {
            let (mut socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0; 4096];
            let (header, offset) = loop {
                let count = socket.read(&mut chunk).await.unwrap();
                assert_ne!(count, 0);
                bytes.extend_from_slice(&chunk[..count]);
                if let Some(offset) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    break (
                        String::from_utf8(bytes[..offset].to_vec()).unwrap(),
                        offset + 4,
                    );
                }
            };
            let length = header
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            while bytes.len() < offset + length {
                let count = socket.read(&mut chunk).await.unwrap();
                assert_ne!(count, 0);
                bytes.extend_from_slice(&chunk[..count]);
            }
            let request = if length == 0 {
                Value::Null
            } else {
                serde_json::from_slice(&bytes[offset..offset + length]).unwrap()
            };
            saved.lock().unwrap().push((header, request));
            let head = format!(
                "HTTP/1.1 {status} Test\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            // Fragment the wire bytes, including inside JSON/tool-argument boundaries.
            for piece in body.as_bytes().chunks(23) {
                socket.write_all(piece).await.unwrap();
            }
        }
    });
    Server {
        url,
        captured,
        task,
    }
}
fn sse(events: Vec<Value>) -> (u16, &'static str, String) {
    let mut body = events
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
    body.push_str("data: [DONE]\n\n");
    (200, "text/event-stream", body)
}
fn text_sse(text: &str) -> (u16, &'static str, String) {
    sse(vec![
        json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":3}}),
    ])
}
fn provider(server: &Server, adapter: AdapterKind) -> Arc<GenaiChatAdapter> {
    Arc::new(
        GenaiChatAdapter::new(
            "plan",
            adapter,
            server.url.clone(),
            AuthData::from_single("fixture-key"),
        )
        .unwrap(),
    )
}

#[tokio::test]
async fn media_and_generation_options_reach_http_and_schema_validates_response() {
    let fixture = server(vec![text_sse("{\"count\":3}")]).await;
    let agent = TestBuilder::default()
        .plugin(
            eliteforge_agent_official_plugin_model_genai::GenaiPlugin::from_adapter(
                provider(&fixture, AdapterKind::OpenAI),
                vec![
                    ModelDescriptor::new("vision")
                        .with_input_modalities([Modality::Text, Modality::Image]),
                ],
            ),
        )
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new(
        "media",
        Content(vec![
            ContentBlock::Text {
                text: "Count items in this image".into(),
            },
            ContentBlock::Media {
                media: MediaContent {
                    mime_type: "image/png".into(),
                    source: MediaSource::Base64("aW1hZ2U=".into()),
                    name: None,
                    estimated_tokens: Some(1000),
                },
            },
        ]),
    );
    request.generation = GenerationOptions {
        temperature: Some(0.3),
        top_p: Some(0.8),
        max_output_tokens: Some(512),
        stop_sequences: Some(vec!["END".into()]),
        output_schema: Some(OutputSchema {
            name: "count".into(),
            schema: json!({"type":"object","properties":{"count":{"type":"integer"}},"required":["count"],"additionalProperties":false}),
        }),
    };
    let result = agent.run(request).await.unwrap();
    assert_eq!(result.structured, Some(json!({"count":3})));
    let calls = fixture.captured.lock().unwrap();
    let body = &calls[0].1;
    assert_eq!(body["temperature"], json!(0.3));
    assert_eq!(body["top_p"], json!(0.8));
    assert_eq!(body["max_tokens"], 512);
    assert_eq!(body["stop"], json!(["END"]));
    assert_eq!(body["response_format"]["type"], "json_schema");
    assert_eq!(body["messages"][0]["content"][0]["type"], "text");
    assert_eq!(
        body["messages"][0]["content"][1]["image_url"]["url"],
        "data:image/png;base64,aW1hZ2U="
    );
}
struct Add;
#[async_trait]
impl Tool for Add {
    async fn call(&self, args: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        assert_eq!(args, json!({"a":19,"b":23}));
        Ok(ToolOutput::text("42"))
    }
}

#[tokio::test]
async fn shared_auth_models_stream_tools_reasoning_and_history_round_trip() {
    let fixture = server(vec![sse(vec![
        json!({"choices":[{"index":0,"delta":{"reasoning_content":"calculate"},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"add","arguments":"{\"a\":19,"}}]},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"b\":23}"}}]},"finish_reason":"tool_calls"}]}),
    ]), text_sse("42"), text_sse("reviewed")]).await;
    let models = provider(&fixture, AdapterKind::OpenAI);
    let memory = TestConversationStore::default();
    let strategy = StageModelStrategy {
        stages: ["planner", "reviewer"]
            .into_iter()
            .map(|name| {
                (
                    name.into(),
                    Arc::new(FixedModelStrategy(ModelRef::new("plan", name)))
                        as Arc<dyn ModelSelectionStrategy>,
                )
            })
            .collect(),
        fallback: None,
    };
    let agent = TestBuilder::default().plugin(eliteforge_agent_official_plugin_model_genai::GenaiPlugin::from_adapter(models, vec![ModelDescriptor::new("planner"), ModelDescriptor::new("reviewer")]))
        .plugin(ModelStrategyPlugin(Arc::new(strategy))).plugin(ConversationStorePlugin::new(memory.clone()))
        .plugin(ToolProviderPlugin::new("tools", StaticToolProvider::new(vec![ToolBinding::new(ToolDescriptor::new("calc.add", "add", "Add", json!({"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}},"required":["a","b"]})), Add)])))
        .build().await.unwrap();
    let mut request = RunRequest::new("s", "19+23");
    request.model_stage = "planner".into();
    let mut handle = agent.start(request).unwrap();
    let mut events = handle.take_events();
    let result = tokio::time::timeout(Duration::from_secs(5), handle.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.output, "42");
    assert_eq!(result.tool_calls, 1);
    assert_eq!(result.usage.input_tokens, 7);
    let mut reasoning = false;
    while let Ok(event) = events.try_recv() {
        if matches!(event.event, AgentEvent::ReasoningDelta(_)) {
            reasoning = true;
        }
    }
    assert!(reasoning);
    assert!(
        memory
            .snapshot("s")
            .iter()
            .any(|m| m.model_context.is_some())
    );
    let mut request = RunRequest::new("s", "review");
    request.model_stage = "reviewer".into();
    assert_eq!(agent.run(request).await.unwrap().output, "reviewed");
    let calls = fixture.captured.lock().unwrap().clone();
    assert_eq!(calls.len(), 3);
    for (headers, _) in calls.iter() {
        assert!(headers.starts_with("POST /v1/chat/completions"));
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("authorization: bearer fixture-key")
        );
    }
    assert_eq!(
        calls
            .iter()
            .map(|c| c.1["model"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["planner", "planner", "reviewer"]
    );
    let assistant = calls[1].1["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "assistant")
        .unwrap();
    assert_eq!(assistant["tool_calls"][0]["id"], "call-1");
    assert_eq!(assistant["reasoning_content"], "calculate");
    assert!(
        calls[1].1["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "tool" && m["content"] == "42")
    );
    // Provider-specific continuation is not forwarded to another model.
    assert!(
        calls[2].1["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m.get("reasoning_content").is_none())
    );
    drop(calls);
    agent.close().await;
}

#[tokio::test]
async fn discovery_uses_same_endpoint_and_auth_and_does_not_invent_capabilities() {
    let fixture = server(vec![(
        200,
        "application/json",
        json!({"data":[{"id":"custom-b"},{"id":"custom-a"},{"id":"custom-a"}]}).to_string(),
    )])
    .await;
    let models = provider(&fixture, AdapterKind::OpenAI)
        .discover()
        .await
        .unwrap();
    assert_eq!(
        models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["custom-a", "custom-b"]
    );
    assert!(
        models
            .iter()
            .all(|m| m.max_input_tokens.is_none() && m.supports_tools.is_none())
    );
    let calls = fixture.captured.lock().unwrap().clone();
    assert!(calls[0].0.starts_with("GET /v1/models"));
    assert!(
        calls[0]
            .0
            .to_ascii_lowercase()
            .contains("authorization: bearer fixture-key")
    );
}

#[tokio::test]
async fn http_errors_retry_only_when_transient_and_redact_payloads() {
    for status in [401, 429] {
        let mut responses = vec![(
            status,
            "application/json",
            json!({"error":{"message":"fixture-secret-payload"}}).to_string(),
        )];
        if status == 429 {
            responses.push(text_sse("recovered"));
        }
        let fixture = server(responses).await;
        let agent = TestBuilder::default()
            .plugin(
                eliteforge_agent_official_plugin_model_genai::GenaiPlugin::from_adapter(
                    provider(&fixture, AdapterKind::OpenAI),
                    vec![ModelDescriptor::new("custom")],
                ),
            )
            .build()
            .await
            .unwrap();
        let result = agent.run(RunRequest::new("s", "test")).await;
        if status == 429 {
            assert_eq!(result.unwrap().output, "recovered");
        } else {
            let error = result.unwrap_err().to_string();
            assert!(error.contains("401"));
            assert!(!error.contains("fixture-secret"));
        }
        assert_eq!(
            fixture.captured.lock().unwrap().len(),
            if status == 429 { 2 } else { 1 }
        );
        agent.close().await;
    }
}

fn anthropic(events: &[(&str, Value)]) -> (u16, &'static str, String) {
    (
        200,
        "text/event-stream",
        events
            .iter()
            .map(|(name, value)| format!("event: {name}\ndata: {value}\n\n"))
            .collect(),
    )
}

#[tokio::test]
async fn partial_stream_failure_does_not_retry_or_switch_models() {
    let fixture = server(vec![sse(vec![
        json!({"choices":[{"index":0,"delta":{"content":"partial"},"finish_reason":null}]}),
        json!({"error":{"type":"server_error","message":"fixture-secret"}}),
    ])])
    .await;
    let agent = TestBuilder::default()
        .plugin(
            eliteforge_agent_official_plugin_model_genai::GenaiPlugin::from_adapter(
                provider(&fixture, AdapterKind::OpenAI),
                vec![ModelDescriptor::new("a"), ModelDescriptor::new("b")],
            ),
        )
        .plugin(ModelStrategyPlugin(Arc::new(RoutingStrategy {
            candidates: vec![ModelRef::new("plan", "a"), ModelRef::new("plan", "b")],
        })))
        .build()
        .await
        .unwrap();
    let result = agent.run(RunRequest::new("s", "test")).await.unwrap_err();
    assert!(!result.to_string().contains("fixture-secret"));
    assert_eq!(fixture.captured.lock().unwrap().len(), 1);
    agent.close().await;
}

#[tokio::test]
async fn cancellation_drops_pending_http_stream_and_releases_session() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/", listener.local_addr().unwrap());
    let (admitted, admission) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = [0; 4096];
        assert!(socket.read(&mut bytes).await.unwrap() > 0);
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
        let _ = admitted.send(());
        loop {
            match socket.read(&mut bytes).await {
                Ok(0) => break,
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => break,
                Ok(_) => {}
                Err(error) => panic!("unexpected socket error: {error}"),
            }
        }
    });
    let provider = Arc::new(
        GenaiChatAdapter::new(
            "plan",
            AdapterKind::OpenAI,
            url,
            AuthData::from_single("fixture-key"),
        )
        .unwrap(),
    );
    let agent = TestBuilder::default()
        .plugin(
            eliteforge_agent_official_plugin_model_genai::GenaiPlugin::from_adapter(
                provider,
                vec![ModelDescriptor::new("a")],
            ),
        )
        .build()
        .await
        .unwrap();
    let handle = agent.start(RunRequest::new("s", "test")).unwrap();
    tokio::time::timeout(Duration::from_secs(3), admission)
        .await
        .unwrap()
        .unwrap();
    handle.cancel();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(3), handle.wait())
            .await
            .unwrap(),
        Err(AgentError::Cancelled)
    ));
    tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .unwrap()
        .unwrap();
    let second = agent.start(RunRequest::new("s", "again")).unwrap();
    second.cancel();
    let _ = second.wait().await;
    agent.close().await;
}

#[tokio::test]
async fn anthropic_signed_thinking_multiple_blocks_round_trip() {
    let fixture = server(vec![anthropic(&[
        ("message_start", json!({"type":"message_start","message":{"id":"m1","usage":{"input_tokens":5,"output_tokens":0}}})),
        ("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}})),
        ("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"reason"}})),
        ("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"signed"}})),
        ("content_block_stop", json!({"type":"content_block_stop","index":0})),
        ("ping", json!({"type":"ping"})),
        ("content_block_start", json!({"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":"second ","signature":"prefix-"}})),
        ("content_block_delta", json!({"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"reason"}})),
        ("content_block_delta", json!({"type":"content_block_delta","index":1,"delta":{"type":"signature_delta","signature":"suffix"}})),
        ("content_block_stop", json!({"type":"content_block_stop","index":1})),
        ("content_block_start", json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"c1","name":"add","input":{}}})),
        ("content_block_delta", json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"a\":19,\"b\":23}"}})),
        ("content_block_stop", json!({"type":"content_block_stop","index":2})),
        ("message_delta", json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":10}})),
        ("message_stop", json!({"type":"message_stop"})),
    ]), anthropic(&[
        ("message_start", json!({"type":"message_start","message":{"id":"m2","usage":{"input_tokens":20,"output_tokens":0}}})),
        ("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})),
        ("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"42"}})),
        ("content_block_stop", json!({"type":"content_block_stop","index":0})),
        ("message_delta", json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}})),
        ("message_stop", json!({"type":"message_stop"})),
    ])]).await;
    let agent = TestBuilder::default()
        .plugin(
            eliteforge_agent_official_plugin_model_genai::GenaiPlugin::from_adapter(
                provider(&fixture, AdapterKind::Anthropic),
                vec![ModelDescriptor::new("custom")],
            ),
        )
        .plugin(ToolProviderPlugin::new(
            "tools",
            StaticToolProvider::new(vec![ToolBinding::new(
                ToolDescriptor::new("calc.add", "add", "Add", json!({"type":"object"})),
                Add,
            )]),
        ))
        .build()
        .await
        .unwrap();
    let result = agent.run(RunRequest::new("s", "test")).await.unwrap();
    assert_eq!(result.output, "42");
    assert_eq!(result.tool_calls, 1);
    let calls = fixture.captured.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    let messages = calls[1].1["messages"].as_array().unwrap();
    let assistant = messages.iter().find(|m| m["role"] == "assistant").unwrap();
    let blocks = assistant["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 3, "no duplicate flattened reasoning");
    assert_eq!(
        blocks[0],
        json!({"type":"thinking","thinking":"reason","signature":"signed"})
    );
    assert_eq!(
        blocks[1],
        json!({"type":"thinking","thinking":"second reason","signature":"prefix-suffix"})
    );
    assert_eq!(blocks[2]["type"], "tool_use");
    assert_eq!(blocks[2]["id"], "c1");
    assert!(messages.iter().any(|m| m["role"] == "user"
        && m["content"].as_array().is_some_and(|parts| {
            parts
                .iter()
                .any(|p| p["type"] == "tool_result" && p["tool_use_id"] == "c1")
        })));
    agent.close().await;
}

#[tokio::test]
async fn anthropic_native_stream_and_auth() {
    let fixture = server(vec![anthropic(&[
        ("message_start", json!({"type":"message_start","message":{"id":"m1","role":"assistant","model":"custom","content":[],"usage":{"input_tokens":5,"output_tokens":0}}})),
        ("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})),
        ("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}})),
        ("content_block_stop", json!({"type":"content_block_stop","index":0})),
        ("message_delta", json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}})),
        ("message_stop", json!({"type":"message_stop"})),
    ])]).await;
    let agent = TestBuilder::default()
        .plugin(
            eliteforge_agent_official_plugin_model_genai::GenaiPlugin::from_adapter(
                provider(&fixture, AdapterKind::Anthropic),
                vec![ModelDescriptor::new("custom")],
            ),
        )
        .build()
        .await
        .unwrap();
    let result = agent.run(RunRequest::new("s", "hi")).await.unwrap();
    assert_eq!(result.output, "hello");
    assert_eq!(result.usage.input_tokens, 5);
    let calls = fixture.captured.lock().unwrap().clone();
    assert!(calls[0].0.starts_with("POST /v1/messages"));
    assert!(
        calls[0]
            .0
            .to_ascii_lowercase()
            .contains("x-api-key: fixture-key")
    );
    assert_eq!(calls[0].1["model"], "custom");
    drop(calls);
    agent.close().await;
}
