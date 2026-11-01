use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_test_support::TestBuilder;
use serde_json::json;
use std::sync::{Arc, Mutex};
fn rich() -> Content {
    Content(vec![
        ContentBlock::Media {
            media: MediaContent {
                mime_type: "image/png".into(),
                source: MediaSource::Url("https://fixture.invalid/image.png".into()),
                name: Some("URL image".into()),
                estimated_tokens: Some(120),
            },
        },
        ContentBlock::Media {
            media: MediaContent {
                mime_type: "image/png".into(),
                source: MediaSource::Base64("AA==".into()),
                name: Some("base64 image".into()),
                estimated_tokens: Some(40),
            },
        },
        ContentBlock::Resource {
            resource: ResourceReference {
                uri: "mcp://authorized/reference".into(),
                mime_type: Some("application/json".into()),
                name: Some("resource".into()),
                size_bytes: Some(17),
            },
        },
        ContentBlock::Json {
            value: json!({"structured":"content"}),
        },
    ])
}
#[derive(Default)]
struct Model(Mutex<Vec<ChatRequest>>);
#[async_trait]
impl ChatModel for Model {
    async fn generate(
        &self,
        _: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        let first = {
            let mut requests = self.0.lock().unwrap();
            requests.push(request);
            requests.len() == 1
        };
        if first {
            Ok(ChatResponse {
                output_items: Vec::new(),
                disposition: None,
                public_content: None,
                reasoning: None,
                content: rich(),
                structured: Some(json!({"structured":"assistant"})),
                tool_calls: vec![ToolCall::new("call", "lookup", json!({}))],
                usage: Usage {
                    input_tokens: 8,
                    output_tokens: 4,
                },
                model_context: Some(ModelMessageContext {
                    model: ModelRef::new("native", "actual"),
                    protocol: "fixture.native".into(),
                    format: "native-history".into(),
                    data: json!({"continuation":"native"}),
                }),
            })
        } else {
            Ok(ChatResponse::text("done"))
        }
    }
}
struct Lookup;
#[async_trait]
impl Tool for Lookup {
    async fn call(&self, _: serde_json::Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("found"))
    }
}
#[tokio::test]
async fn second_native_dispatch_retains_non_tool_media_resource_structure_and_model_context() {
    let model = Arc::new(Model::default());
    let mut descriptor = ModelDescriptor::new("actual");
    descriptor.supports_tools = Some(true);
    descriptor.input_modalities = vec![Modality::Text, Modality::Image];
    descriptor.output_modalities = vec![Modality::Text, Modality::Image];
    let runtime = ModelRuntime::new(ModelRuntimeUpdate::new(
        "native",
        1,
        vec![
            ModelConnection::new("native", model.clone(), vec![descriptor])
                .with_protocol("fixture.native"),
        ],
    ))
    .unwrap();
    let agent = TestBuilder::default()
        .default_models(false)
        .plugin(ModelRuntimePlugin::new(runtime))
        .plugin(ToolProviderPlugin::new(
            "tools",
            StaticToolProvider::new(vec![ToolBinding::new(
                ToolDescriptor::new("lookup", "lookup", "lookup", json!({"type":"object"})),
                Lookup,
            )]),
        ))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("rich-history", "start");
    request.model = Some(ModelRef::new("native", "actual"));
    assert_eq!(agent.run(request).await.unwrap().output, "done");
    {
        let requests = model.0.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let assistant = requests[1]
            .messages
            .iter()
            .find(|message| message.role == Role::Assistant)
            .unwrap();
        assert_eq!(assistant.content, rich());
        assert_eq!(
            assistant.structured,
            Some(json!({"structured":"assistant"}))
        );
        assert_eq!(
            assistant.model_context.as_ref().unwrap().model,
            ModelRef::new("native", "actual")
        );
        assert_eq!(
            assistant.model_context.as_ref().unwrap().format,
            "native-history"
        );
        assert_eq!(assistant.tool_calls[0].id, "call");
    }
    assert!(agent.framework().close().await.is_success());
}
