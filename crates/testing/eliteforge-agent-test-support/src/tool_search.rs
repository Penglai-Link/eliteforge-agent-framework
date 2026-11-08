//! Shared scenario for framework and UI contract regressions.
use eliteforge_agent_framework::*;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub struct TwoStageSearchModel(pub Arc<Mutex<Vec<ChatRequest>>>);
#[async_trait::async_trait]
impl ChatModel for TwoStageSearchModel {
    async fn generate(
        &self,
        _: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        let step = request
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .count();
        self.0.lock().unwrap().push(request);
        let (id, name, arguments) = match step {
            0 => (
                "summaries",
                TOOL_SEARCH_CALL_NAME,
                json!({"items":[{"domainCode":"sample"},{"query":"absent"}]}),
            ),
            1 => (
                "definitions",
                TOOL_SEARCH_CALL_NAME,
                json!({"items":[
                    {"tool":"sample_read"}, {"tool":"sample_edit"}, {"tool":"missing"}, {"tool":"sample_hidden"}
                ]}),
            ),
            2 => (
                "execute",
                TOOL_EXECUTE_CALL_NAME,
                json!({"tool":"sample_read","arguments":{"items":[{"name":"record"}]}}),
            ),
            _ => return Ok(ChatResponse::text("done")),
        };
        Ok(ChatResponse::tool_calls(vec![ToolCall::new(
            id, name, arguments,
        )]))
    }
}
struct Echo;
#[async_trait::async_trait]
impl Tool for Echo {
    async fn call(&self, value: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text(value["name"].as_str().unwrap()))
    }
}
pub fn two_stage_bindings() -> Vec<ToolBinding> {
    let descriptor = ToolDescriptor::new(
        "sample.read",
        "sample_read",
        "Read a sample record",
        json!({
            "type":"object", "required":["name"], "additionalProperties":false,
            "$defs":{"name":{"type":"string","minLength":1}},
            "properties":{"name":{"$ref":"#/$defs/name"}}, "examples":[{"name":"record"}]
        }),
    );
    vec![
        ToolBinding::new(descriptor, Echo).with_domain_code("sample"),
        ToolBinding::new(
            ToolDescriptor::new(
                "sample.edit",
                "sample_edit",
                "Edit a sample record",
                json!({
                    "type":"object","required":["name"],"properties":{"name":{"type":"string"}}
                }),
            )
            .with_single_input(),
            Echo,
        )
        .with_domain_code("sample"),
        ToolBinding::new(
            ToolDescriptor::new(
                "sample.hidden",
                "sample_hidden",
                "Hidden sample",
                json!({"type":"object"}),
            ),
            Echo,
        )
        .hidden(),
    ]
}
pub fn assert_two_stage_requests(model: &TwoStageSearchModel) {
    let requests = model.0.lock().unwrap();
    assert_eq!(requests.len(), 4);
    for request in requests.iter() {
        assert_eq!(request.tools, requests[0].tools);
        assert_eq!(
            request
                .tools
                .iter()
                .map(|t| t.call_name.as_str())
                .collect::<Vec<_>>(),
            [TOOL_EXECUTE_CALL_NAME, TOOL_SEARCH_CALL_NAME]
        );
    }
    let output = |step: usize| -> Value {
        serde_json::from_str(
            &requests[step]
                .messages
                .iter()
                .rev()
                .find(|m| m.role == Role::Tool)
                .unwrap()
                .content
                .text(),
        )
        .unwrap()
    };
    for request in requests.iter().skip(1) {
        let message = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Tool)
            .unwrap();
        assert!(message.tool_result.as_ref().unwrap().structured.is_none());
    }
    let summary = output(1);
    let summary_result = &summary["results"][0]["result"];
    assert_eq!(summary_result["kind"], "toolSearch");
    assert_eq!(summary_result["mode"], "summary");
    assert_eq!(summary_result["matched"], 2);
    assert_eq!(summary["results"][1]["result"]["matched"], 0);
    assert!(!summary.to_string().contains("input_schema"));
    assert!(!summary.to_string().contains("Batch operation"));
    assert!(!summary.to_string().contains("sample_hidden"));
    let definitions = output(2);
    assert_eq!(definitions["results"].as_array().unwrap().len(), 4);
    for index in [0, 1] {
        assert_eq!(
            definitions["results"][index]["result"]["mode"],
            "definition"
        );
    }
    let schema = &definitions["results"][0]["result"]["tools"][0]["input_schema"];
    let validator = compile_schema(schema).unwrap();
    assert!(validator.is_valid(&json!({"items":[{"name":"record"}]})));
    assert!(!validator.is_valid(&json!({"items":[{"name":3}]})));
    assert_eq!(
        definitions["results"][1]["result"]["tools"][0]["input_schema"]["required"],
        json!(["name"])
    );
    assert_eq!(definitions["results"][2]["isError"], true);
    assert_eq!(definitions["results"][3]["isError"], true);
    assert_eq!(output(3)["results"][0]["result"], "record");
}
