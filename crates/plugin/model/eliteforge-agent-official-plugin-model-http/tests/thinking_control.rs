use eliteforge_agent_official_plugin_model_http::{
    AgentMessage, AgentModelConfig, ModelRequest,
    protocol::{ChatCompletionsAdapter, ModelProtocolAdapter, ResponsesAdapter},
};
use serde_json::json;

#[test]
fn chat_requests_only_override_thinking_when_enabled() {
    let request = ModelRequest {
        messages: vec![AgentMessage::user("Generate a title")],
        tools: vec![],
        prepared_tool_schema: None,
        system_agent_session_id: None,
        response_input_items: None,
        previous_response_id: None,
    };
    for enabled in [false, true] {
        let config: AgentModelConfig = serde_json::from_value(json!({
            "modelCode":"fixture", "protocol":"CHAT_COMPLETIONS",
            "baseUrl":"https://unused.invalid", "apiKey":"", "modelName":"fixture",
            "thinkingEnabled":enabled, "reasoningEffort":"High"
        }))
        .unwrap();
        for body in [
            ChatCompletionsAdapter
                .request_body(&config, &request)
                .unwrap(),
            ChatCompletionsAdapter
                .stream_request_body(&config, &request)
                .unwrap(),
        ] {
            if enabled {
                assert_eq!(body["enable_thinking"], json!(true));
                assert_eq!(body["thinking"]["type"], "enabled");
                assert_eq!(body["reasoning_effort"], "high");
            } else {
                assert!(body.get("enable_thinking").is_none());
                assert!(body.get("thinking").is_none());
                assert!(body.get("reasoning_effort").is_none());
            }
        }
    }
}

#[test]
fn raw_reasoning_is_never_promoted_to_answer_text() {
    let adapter = ChatCompletionsAdapter;
    assert!(adapter.parse_response(&json!({
        "choices":[{"message":{"role":"assistant","reasoning_content":"Private analysis"},"finish_reason":"stop"}]
    })).is_err());
    let answer = adapter.parse_response(&json!({
        "choices":[{"message":{"role":"assistant","content":"Tron architecture","reasoning_content":"Private analysis"},"finish_reason":"stop"}]
    })).unwrap();
    assert_eq!(answer.output_text(), "Tron architecture");
}

#[test]
fn responses_requests_only_override_thinking_when_enabled() {
    let request: ModelRequest = serde_json::from_value(json!({
        "messages":[{"role":"user","parts":[{"type":"text","text":"Generate a title"}]}], "tools":[]
    }))
    .unwrap();
    for (enabled, effort) in [(false, "None"), (true, "High")] {
        let config: AgentModelConfig = serde_json::from_value(json!({
            "modelCode":"fixture", "protocol":"RESPONSES",
            "baseUrl":"https://unused.invalid", "apiKey":"", "modelName":"fixture",
            "thinkingEnabled":enabled, "reasoningEffort":effort
        }))
        .unwrap();
        for body in [
            ResponsesAdapter.request_body(&config, &request).unwrap(),
            ResponsesAdapter
                .stream_request_body(&config, &request)
                .unwrap(),
        ] {
            if enabled {
                assert_eq!(body["reasoning"]["effort"], "high");
            } else {
                assert!(body.get("reasoning").is_none());
            }
        }
    }
}
