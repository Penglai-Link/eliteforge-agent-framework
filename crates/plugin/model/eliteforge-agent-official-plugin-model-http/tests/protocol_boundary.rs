use eliteforge_agent_core as fw;
use eliteforge_agent_official_plugin_model_http::*;
use serde_json::json;
use std::sync::{Arc, Mutex};
#[test]
fn sse_handles_every_utf8_byte_split_crlf_and_multiline_data() {
    let wire = "data: {\r\ndata: \"text\":\"你好🌍\"}\r\n\r\ndata: [DONE]\r\n\r\n".as_bytes();
    for split in 0..=wire.len() {
        let events = Arc::new(Mutex::new(vec![]));
        let capture = events.clone();
        let sink: EventSink<'_> = Arc::new(move |value| {
            capture.lock().unwrap().push(value);
            Ok(())
        });
        let mut decoder = SseDecoder::new(512);
        decoder.feed(&wire[..split], &sink).unwrap();
        decoder.feed(&wire[split..], &sink).unwrap();
        decoder.finish(&sink).unwrap();
        assert_eq!(*events.lock().unwrap(), [json!({"text":"你好🌍"})]);
    }
}
#[test]
fn sse_bounds_unfinished_frames_and_rejects_invalid_utf8_and_truncation() {
    let sink: EventSink<'_> = Arc::new(|_| Ok(()));
    for input in [b"data: \xff\n\n".as_slice(), b"data: {}".as_slice()] {
        let mut decoder = SseDecoder::new(64);
        assert!(
            decoder
                .feed(input, &sink)
                .and_then(|_| decoder.finish(&sink))
                .is_err()
        );
    }
    let mut decoder = SseDecoder::new(32);
    assert!(decoder.feed(&[b'x'; 33], &sink).is_err());
}
#[test]
fn errors_preserve_categories_business_codes_and_plain_body_policies() {
    for (status, value, category) in [
        (
            401,
            json!({"message":"设备已解绑"}),
            fw::ProviderErrorCategory::AccountChanged,
        ),
        (
            429,
            json!({"message":"insufficient quota"}),
            fw::ProviderErrorCategory::Quota,
        ),
        (
            400,
            json!({"code":17,"message":"invalid request"}),
            fw::ProviderErrorCategory::Other,
        ),
        (
            401,
            json!({"message":"expired"}),
            fw::ProviderErrorCategory::Authentication,
        ),
        (
            403,
            json!({"error":{"message":"permission"}}),
            fw::ProviderErrorCategory::Permission,
        ),
    ] {
        let error = status_error(status, &value);
        let encoded = encode_model_error(&error);
        let decoded = decode_model_error(&encoded).unwrap();
        let fw::ModelError::Provider(error) = decoded else {
            panic!()
        };
        assert_eq!(error.category, category);
        assert_eq!(
            error.business_code,
            value.get("code").and_then(|v| v.as_i64()).map(|n| n as i32)
        );
        assert!(!fw::ModelError::Provider(error).is_transient());
    }
}
#[test]
fn legacy_tool_history_is_typed_data_and_never_panics() {
    let request = ModelRequest {
        messages: vec![
            AgentMessage::new(
                "assistant",
                vec![AgentMessagePart::ToolCall {
                    call_id: None,
                    tool_name: "test".into(),
                    input: json!({}),
                    input_error: None,
                }],
            ),
            AgentMessage::new(
                "tool",
                vec![AgentMessagePart::ToolResult {
                    call_id: None,
                    tool_name: "test".into(),
                    output: "ok".into(),
                    externalized: false,
                    metadata: None,
                }],
            ),
        ],
        tools: vec![],
        prepared_tool_schema: None,
        system_agent_session_id: None,
        response_input_items: None,
        previous_response_id: None,
    };
    let encoded = request_to_framework(request).unwrap();
    assert_eq!(
        encoded.messages[0].tool_calls[0].id,
        encoded.messages[1].tool_call_id.clone().unwrap()
    );
}

#[tokio::test]
async fn model_and_probe_redirects_never_post_to_target_even_with_host_follow_policy() {
    use eliteforge_agent_official_plugin_model_http::{
        HttpModelTransport, ModelHttpClient, ModelTransport,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for status in [307, 308] {
        for probe in [false, true] {
            let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let destination = format!("http://{}/redirected", target.local_addr().unwrap());
            let original = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", original.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut socket, _) = original.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0; 1024];
                loop {
                    let n = socket.read(&mut buffer).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buffer[..n]);
                    if bytes.windows(4).any(|v| v == b"\r\n\r\n") {
                        break;
                    }
                }
                let body = "{}";
                socket.write_all(format!("HTTP/1.1 {status} Redirect\r\nLocation: {destination}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{body}").as_bytes()).await.unwrap();
                assert!(
                    tokio::time::timeout(std::time::Duration::from_millis(100), original.accept())
                        .await
                        .is_err()
                );
            });
            let client = ModelHttpClient::from_builder(
                reqwest::Client::builder().redirect(reqwest::redirect::Policy::limited(10)),
            )
            .unwrap();
            let transport = HttpModelTransport::new(client, url, "fixture");
            let body = serde_json::json!({"model":"fixture","messages":[{"role":"user","content":"data"}]});
            let result = if probe {
                transport.probe("/chat/completions", &body).await
            } else {
                transport.json("/chat/completions", &body).await
            };
            assert!(result.is_err());
            server.await.unwrap();
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(100), target.accept())
                    .await
                    .is_err()
            );
        }
    }
}
