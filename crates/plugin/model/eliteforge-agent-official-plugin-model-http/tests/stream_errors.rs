use async_trait::async_trait;
use eliteforge_agent_core::ModelError;
use eliteforge_agent_official_plugin_model_http::*;
use serde_json::{Value, json};
use std::sync::Arc;

struct StreamTransport(Vec<Value>);

#[async_trait]
impl ModelTransport for StreamTransport {
    async fn json(&self, _: &str, _: &Value) -> Result<Value, ModelError> {
        panic!("stream generation must not fall back to a JSON request");
    }

    async fn events(&self, _: &str, _: &Value, sink: EventSink<'_>) -> Result<(), ModelError> {
        for event in &self.0 {
            sink(event.clone())?;
        }
        Ok(())
    }
}

async fn generate(events: Vec<Value>) -> Result<ModelResponse, ModelError> {
    let config = serde_json::from_value(json!({
        "modelCode": "fixture",
        "protocol": "RESPONSES",
        "baseUrl": "https://unused.invalid",
        "apiKey": "",
        "modelName": "fixture"
    }))
    .unwrap();
    let request = ModelRequest {
        messages: vec![AgentMessage::user("Reply OK")],
        tools: vec![],
        prepared_tool_schema: None,
        system_agent_session_id: None,
        response_input_items: None,
        previous_response_id: None,
    };
    HttpProtocolClient::new(Arc::new(StreamTransport(events)))
        .generate_stream(&config, &request, Arc::new(|_| {}))
        .await
}

async fn failure_details(event: Value) -> Value {
    // A later successful terminal event must not hide an upstream failure.
    let error = generate(vec![event, completed()]).await.unwrap_err();
    assert!(!error.is_transient());
    let decoded = decode_model_error(&encode_model_error(&error)).unwrap();
    let ModelError::Indeterminate(message) = decoded else {
        panic!("stream failures must retain their existing recovery classification");
    };
    serde_json::from_str(
        message
            .strip_prefix("model stream failed before completion: ")
            .unwrap(),
    )
    .unwrap()
}

fn completed() -> Value {
    json!({
        "type": "response.completed",
        "error": null,
        "response": {
            "id": "response-fixture",
            "status": "completed",
            "error": null,
            "output_text": "OK"
        }
    })
}

#[tokio::test]
async fn stream_errors_preserve_nested_flat_and_non_object_provider_details() {
    for event in [
        json!({"error": {
            "code": "rate_limit_exceeded", "message": "请求太频繁，请稍后重试",
            "type": "requests", "param": "model", "retry_after": 15
        }}),
        json!({"type": "error", "code": "server_error", "message": "upstream failed",
            "param": null, "sequence_number": 8}),
        json!({"type": "error", "error": "provider disconnected"}),
        json!({"error": {"code": 17, "message": "business failure"}}),
    ] {
        assert_eq!(failure_details(event.clone()).await, event);
    }
}

#[tokio::test]
async fn failed_responses_preserve_error_codes_and_details_without_response_content() {
    let error = json!({
        "code": "server_error", "message": "generation failed",
        "type": "provider_error", "details": {"request_id": "request-fixture"}
    });
    let details = failure_details(json!({
        "type": "response.failed",
        "sequence_number": 12,
        "response": {
            "id": "response-fixture", "status": "failed", "error": error,
            "output": [{"content": "partial output"}],
            "output_text": "partial output", "instructions": "request instructions"
        }
    }))
    .await;
    assert_eq!(
        details,
        json!({
            "type": "response.failed", "sequence_number": 12,
            "response": {"id": "response-fixture", "status": "failed", "error": error}
        })
    );
}

#[tokio::test]
async fn incomplete_responses_preserve_reasons_and_additional_details() {
    for reason in ["max_output_tokens", "content_filter"] {
        let incomplete = json!({"reason": reason, "details": {"limit": 2048}});
        let details = failure_details(json!({
            "type": "response.incomplete",
            "response": {
                "id": "response-fixture", "status": "incomplete", "error": null,
                "incomplete_details": incomplete, "output_text": "partial output"
            }
        }))
        .await;
        assert_eq!(details["type"], "response.incomplete");
        assert_eq!(details["response"]["incomplete_details"], incomplete);
        assert!(details["response"].get("output_text").is_none());
    }
}

#[tokio::test]
async fn failures_without_details_still_report_the_event_type() {
    for kind in ["error", "response.failed", "response.incomplete"] {
        let event = json!({"type": kind});
        assert_eq!(failure_details(event.clone()).await, event);
    }
}

#[tokio::test]
async fn null_error_does_not_reject_a_successful_response() {
    let response = generate(vec![completed()]).await.unwrap();
    assert_eq!(response.assistant.text_content(), "OK");
}

#[tokio::test]
async fn explicit_model_rejection_allows_recovery_only_before_any_stream_event() {
    let rejection = json!({"error":{"type":"gateway_error","message":"Stream chat completion failed: The model does not exist or you do not have access to it."}});
    let error = generate(vec![rejection.clone()]).await.unwrap_err();
    assert!(matches!(error, ModelError::Unavailable(message) if message.contains("gateway_error")));
    let error = generate(vec![
        json!({"type":"response.output_text.delta","delta":"partial"}),
        rejection,
    ])
    .await
    .unwrap_err();
    assert!(matches!(error, ModelError::Indeterminate(_)));
}
