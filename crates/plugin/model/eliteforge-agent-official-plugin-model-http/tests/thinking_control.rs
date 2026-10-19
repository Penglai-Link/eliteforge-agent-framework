use eliteforge_agent_official_plugin_model_http::{
    AgentMessage, AgentModelConfig, ModelRequest,
    protocol::{ChatCompletionsAdapter, ModelProtocolAdapter, ResponsesAdapter},
};
use serde_json::json;

#[test]
fn chat_requests_only_override_thinking_when_enabled() {
    let request = ModelRequest {
        messages: vec![AgentMessage::system("trusted host rule"), AgentMessage::user("Generate a title")],
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
            "thinkingEnabled":enabled, "reasoningEffort":"High", "maxTokens":32768
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
            assert_eq!(body["messages"][0]["role"], "system");
            assert_eq!(body["messages"][0]["content"], "trusted host rule");
            assert_eq!(body["messages"][1]["role"], "user");
            assert_eq!(body["max_tokens"], 32768);
            assert_eq!(body["tool_choice"], "none");
            assert!(body.get("max_output_tokens").is_none());
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
fn unsupported_response_diagnoses_output_exhaustion_without_exposing_provider_content() {
    let error = ChatCompletionsAdapter.parse_response(&json!({
        "id":"private-response-id","model":"private-model",
        "choices":[{"message":{"role":"assistant","content":null,"reasoning_content":"private reasoning"},"finish_reason":"length"}],
        "usage":{"completion_tokens":16384,"completion_tokens_details":{"reasoning_tokens":16384}}
    })).unwrap_err();
    assert!(error.contains("finish=Length"));
    assert!(error.contains("content=Null"));
    assert!(error.contains("reasoning=true"));
    assert!(error.contains("completionTokens=Some(16384)"));
    assert!(error.contains("reasoningTokens=Some(16384)"));
    assert!(!error.contains("private"));
    let unknown = ChatCompletionsAdapter.parse_response(&json!({"choices":[{"message":{"content":null},"finish_reason":"private-value"}],"error":{"message":"private-error"}})).unwrap_err();
    assert!(unknown.contains("finish=Other"));
    assert!(unknown.contains("completionTokens=None"));
    assert!(!unknown.contains("private"));
}

#[test]
fn responses_requests_only_override_thinking_when_enabled() {
    let request: ModelRequest = serde_json::from_value(json!({
        "messages":[{"role":"system","parts":[{"type":"text","text":"trusted host rule"}]},{"role":"user","parts":[{"type":"text","text":"Generate a title"}]}], "tools":[]
    }))
    .unwrap();
    for (enabled, effort) in [(false, "None"), (true, "High")] {
        let config: AgentModelConfig = serde_json::from_value(json!({
            "modelCode":"fixture", "protocol":"RESPONSES",
            "baseUrl":"https://unused.invalid", "apiKey":"", "modelName":"fixture",
            "thinkingEnabled":enabled, "reasoningEffort":effort, "maxTokens":32768
        }))
        .unwrap();
        for body in [
            ResponsesAdapter.request_body(&config, &request).unwrap(),
            ResponsesAdapter
                .stream_request_body(&config, &request)
                .unwrap(),
        ] {
            assert_eq!(body["instructions"], "trusted host rule");
            assert_eq!(body["input"], "Generate a title");
            assert_eq!(body["max_output_tokens"], 32768);
            assert_eq!(body["tool_choice"], "none");
            assert!(body.get("max_tokens").is_none());
            if enabled {
                assert_eq!(body["reasoning"]["effort"], "high");
            } else {
                assert!(body.get("reasoning").is_none());
            }
        }
    }
}

#[test]
fn configured_tools_keep_their_selection_and_parallel_calls() {
    use eliteforge_agent_official_plugin_model_http::EffectiveTool;
    let request = ModelRequest {
        messages: vec![AgentMessage::user("Look up the source")],
        tools: vec![EffectiveTool {
            tool_name: "lookup".into(),
            description: "Read evidence".into(),
            input_schema: json!({"type":"object","properties":{"query":{"type":"string"}}}),
        }],
        prepared_tool_schema: None,
        system_agent_session_id: None,
        response_input_items: None,
        previous_response_id: None,
    };
    for adapter in [
        &ChatCompletionsAdapter as &dyn ModelProtocolAdapter,
        &ResponsesAdapter,
    ] {
        let config: AgentModelConfig = serde_json::from_value(json!({
            "modelCode":"fixture", "protocol":adapter.protocol(),
            "baseUrl":"https://unused.invalid", "apiKey":"", "modelName":"fixture"
        }))
        .unwrap();
        for body in [
            adapter.request_body(&config, &request).unwrap(),
            adapter.stream_request_body(&config, &request).unwrap(),
        ] {
            assert_eq!(body["tools"].as_array().unwrap().len(), 1);
            assert_eq!(body["parallel_tool_calls"], true);
            assert!(body.get("tool_choice").is_none());
        }
    }
}
