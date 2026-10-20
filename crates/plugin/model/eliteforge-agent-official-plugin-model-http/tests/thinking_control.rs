use eliteforge_agent_core::InstructionOrigin;
use eliteforge_agent_official_plugin_model_http::{
    AgentMessage, AgentModelConfig, ModelRequest,
    protocol::{ChatCompletionsAdapter, ModelProtocolAdapter, ResponsesAdapter},
};
use serde_json::json;

#[test]
fn chat_requests_only_override_thinking_when_enabled() {
    let request = ModelRequest {
        messages: vec![
            AgentMessage::system("trusted host rule"),
            AgentMessage::user("Generate a title"),
        ],
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
fn chat_instruction_role_is_an_explicit_per_model_choice() {
    let mut agent_instruction = AgentMessage::system("agent custom rule");
    agent_instruction.instruction_origin = Some(InstructionOrigin::Agent);
    let mut plugin_instruction = AgentMessage::system("plugin rule");
    plugin_instruction.instruction_origin = Some(InstructionOrigin::Plugin);
    let request = ModelRequest {
        messages: vec![
            AgentMessage::system("internal rule"),
            agent_instruction,
            AgentMessage::user("Generate a title"),
            plugin_instruction,
        ],
        tools: vec![],
        prepared_tool_schema: None,
        system_agent_session_id: None,
        response_input_items: None,
        previous_response_id: None,
    };
    for (configured, expected_role) in [
        (None, "system"),
        (Some(false), "system"),
        (Some(true), "developer"),
    ] {
        let mut value = json!({
            "modelCode":"fixture", "protocol":"CHAT_COMPLETIONS",
            "baseUrl":"https://unused.invalid", "apiKey":"", "modelName":"fixture"
        });
        if let Some(enabled) = configured {
            value["supportsDeveloperRole"] = json!(enabled);
        }
        let config: AgentModelConfig = serde_json::from_value(value).unwrap();
        for body in [
            ChatCompletionsAdapter
                .request_body(&config, &request)
                .unwrap(),
            ChatCompletionsAdapter
                .stream_request_body(&config, &request)
                .unwrap(),
        ] {
            assert_eq!(
                body["messages"][0],
                json!({"role":"system","content":"internal rule"})
            );
            assert_eq!(
                body["messages"][1],
                json!({"role":expected_role,"content":"agent custom rule"})
            );
            assert_eq!(
                body["messages"][2],
                json!({"role":"user","content":"Generate a title"})
            );
            assert_eq!(
                body["messages"][3],
                json!({"role":expected_role,"content":"plugin rule"})
            );
        }
    }
}

#[test]
fn framework_instruction_origins_survive_the_http_model_bridge() {
    use eliteforge_agent_official_plugin_model_http::{
        request_from_framework, request_to_framework,
    };
    let request = eliteforge_agent_core::ChatRequest {
        messages: vec![
            eliteforge_agent_core::Message::system("internal rule"),
            eliteforge_agent_core::Message::system("agent custom rule")
                .with_instruction_origin(InstructionOrigin::Agent),
            eliteforge_agent_core::Message::system("plugin rule")
                .with_instruction_origin(InstructionOrigin::Plugin),
        ],
        tools: vec![],
        extensions: Default::default(),
        options: Default::default(),
    };
    let encoded = request_from_framework(&request).unwrap();
    assert_eq!(encoded.messages[0].instruction_origin, None);
    assert_eq!(
        encoded.messages[1].instruction_origin,
        Some(InstructionOrigin::Agent)
    );
    assert_eq!(
        encoded.messages[2].instruction_origin,
        Some(InstructionOrigin::Plugin)
    );
    let restored = request_to_framework(encoded).unwrap();
    assert_eq!(restored.messages, request.messages);
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
            assert!(body.get("instructions").is_none());
            assert_eq!(body["input"][0]["role"], "system");
            assert_eq!(body["input"][0]["content"], "trusted host rule");
            assert_eq!(body["input"][1]["role"], "user");
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
fn responses_promotes_only_marked_instructions_when_supported() {
    let mut agent = AgentMessage::system("agent custom rule");
    agent.instruction_origin = Some(InstructionOrigin::Agent);
    let mut plugin = AgentMessage::system("plugin rule");
    plugin.instruction_origin = Some(InstructionOrigin::Plugin);
    let request = ModelRequest {
        messages: vec![AgentMessage::system("internal rule"), agent, plugin, AgentMessage::user("task")],
        tools: vec![], prepared_tool_schema: None, system_agent_session_id: None,
        response_input_items: None, previous_response_id: None,
    };
    for supported in [false, true] {
        let config: AgentModelConfig = serde_json::from_value(json!({
            "modelCode":"fixture", "protocol":"RESPONSES", "baseUrl":"https://unused.invalid",
            "apiKey":"", "modelName":"fixture", "supportsDeveloperRole":supported
        })).unwrap();
        let body = ResponsesAdapter.request_body(&config, &request).unwrap();
        if supported {
            assert_eq!(body["instructions"], "agent custom rule\n\nplugin rule");
            assert_eq!(body["input"][0]["content"], "internal rule");
            assert_eq!(body["input"][1]["role"], "user");
        } else {
            assert!(body.get("instructions").is_none());
            assert_eq!(body["input"].as_array().unwrap().len(), 4);
            assert_eq!(body["input"][1]["role"], "system");
        }
    }
}

#[test]
fn responses_stateful_delta_keeps_system_messages() {
    let request = ModelRequest {
        messages: vec![AgentMessage::system("internal rule"), AgentMessage::user("task")],
        tools: vec![], prepared_tool_schema: None, system_agent_session_id: None,
        response_input_items: Some(vec![json!({"type":"message","role":"user","content":"task"})]),
        previous_response_id: Some("resp-1".into()),
    };
    let config: AgentModelConfig = serde_json::from_value(json!({
        "modelCode":"fixture", "protocol":"RESPONSES", "baseUrl":"https://unused.invalid",
        "apiKey":"", "modelName":"fixture", "supportsDeveloperRole":true
    })).unwrap();
    let body = ResponsesAdapter.request_body(&config, &request).unwrap();
    assert_eq!(body["input"][0]["role"], "system");
    assert_eq!(body["input"][1]["role"], "user");
    assert_eq!(body["previous_response_id"], "resp-1");
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
