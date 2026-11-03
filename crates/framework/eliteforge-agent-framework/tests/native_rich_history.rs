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

/// A detached child may settle its parent's budget after its launching tool
/// has returned. Retain real run contexts rather than fabricating revisions.
#[derive(Clone, Default)]
struct DeferredAccounting(Arc<Mutex<Vec<RunContext>>>);
#[async_trait]
impl Tool for DeferredAccounting {
    async fn call(
        &self,
        _: serde_json::Value,
        context: &RunContext,
    ) -> Result<ToolOutput, ToolError> {
        self.0.lock().unwrap().push(context.clone());
        Ok(ToolOutput::text("work already committed"))
    }
}
struct Pause;
#[async_trait]
impl Tool for Pause {
    async fn call(&self, _: serde_json::Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("confirm").request_input(
            InteractionRequest::input("continue?", json!({"type":"boolean"})).unwrap(),
        ))
    }
}
struct AccountingModel {
    accounting: DeferredAccounting,
    calls: std::sync::atomic::AtomicUsize,
    work_count: usize,
    pause: bool,
}
#[async_trait]
impl ChatModel for AccountingModel {
    async fn generate(
        &self,
        _: &str,
        _: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        let index = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut response = if self.pause && index == 0 {
            ChatResponse {
                tool_calls: vec![ToolCall::new("pause", "pause", json!({}))],
                ..Default::default()
            }
        } else if index == usize::from(self.pause) {
            ChatResponse {
                tool_calls: (0..self.work_count)
                    .map(|i| ToolCall::new(format!("start-work-{i}"), "defer", json!({})))
                    .collect(),
                ..Default::default()
            }
        } else {
            let earlier_tools = self.accounting.0.lock().unwrap().clone();
            futures::future::try_join_all(earlier_tools.iter().map(|earlier| async move {
                let (mut usage, _) = budget::UsagePermit::reserve_child(
                    earlier.usage.clone(),
                    7,
                    RunLimits::UNLIMITED_TOKENS,
                )?;
                usage.settle_child(
                    &Usage {
                        input_tokens: 7,
                        output_tokens: 0,
                    },
                    RunLimits::UNLIMITED_TOKENS,
                )?;
                // History and reservation for this model attempt have already
                // advanced the checkpoint. Late child settlement shares its head.
                earlier.persist_budget().await.map_err(ModelError::from)
            }))
            .await?;
            ChatResponse::text("done without repeating work")
        };
        response.usage = Usage {
            input_tokens: 10,
            output_tokens: 2,
        };
        Ok(response)
    }
}
#[tokio::test]
async fn deferred_tool_accounting_uses_current_checkpoint_after_next_model_attempt_starts() {
    use eliteforge_agent_official_plugin_storage_local::{SqliteOptions, SqliteStateStore};
    for (work_count, pause) in [(1, false), (2, false), (2, true)] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.sqlite3");
        let store = Arc::new(
            SqliteStateStore::open(&path, SqliteOptions::default())
                .await
                .unwrap(),
        );
        let accounting = DeferredAccounting::default();
        let agent = TestBuilder::default()
            .plugin(store.clone().plugin())
            .plugin(ChatModelPlugin::new(AccountingModel {
                accounting: accounting.clone(),
                calls: Default::default(),
                work_count,
                pause,
            }))
            .plugin(ToolProviderPlugin::new(
                "deferred-accounting",
                StaticToolProvider::new(vec![
                    ToolBinding::new(
                        ToolDescriptor::new(
                            "defer",
                            "defer",
                            "start durable work",
                            json!({"type":"object"}),
                        ),
                        accounting.clone(),
                    )
                    .parallel_safe(),
                    ToolBinding::new(
                        ToolDescriptor::new(
                            "pause",
                            "pause",
                            "confirm work",
                            json!({"type":"object"}),
                        ),
                        Pause,
                    ),
                ]),
            ))
            .build()
            .await
            .unwrap();
        let first = agent
            .run(RunRequest::new("deferred-accounting", "go"))
            .await
            .unwrap();
        let result = if pause {
            assert_eq!(first.status, RunStatus::AwaitingInput);
            let request = agent
                .answer("deferred-accounting", first.interactions[0].id, json!(true))
                .await
                .unwrap();
            agent.run(request).await.unwrap()
        } else {
            first
        };
        assert_eq!(result.output, "done without repeating work");
        let calls = work_count + usize::from(pause);
        assert_eq!(result.tool_calls, calls);
        assert_eq!(result.steps, 2 + usize::from(pause));
        let checkpoint = store.latest("deferred-accounting").await.unwrap().unwrap();
        assert_eq!(checkpoint.status, ExecutionStatus::Completed);
        assert_eq!(checkpoint.history.tool_invocations.len(), calls);
        assert!(
            checkpoint
                .history
                .steps
                .iter()
                .all(|step| step.status == HistoryStatus::Completed)
        );
        assert!(
            checkpoint
                .history
                .tool_invocations
                .iter()
                .all(|call| call.status == HistoryStatus::Completed)
        );
        assert_eq!(
            checkpoint
                .transcript
                .iter()
                .filter(|message| message.role == Role::Tool)
                .count(),
            calls
        );
        assert_eq!(
            checkpoint.state["budget_usage"]["input_tokens"],
            10 * result.steps + 7 * work_count
        );
        assert_eq!(
            accounting.0.lock().unwrap().len(),
            work_count,
            "successful tools must not replay"
        );
        let stale = accounting.0.lock().unwrap()[0].clone();
        assert!(
            stale.persist_budget().await.is_err(),
            "terminal/expired owners must still be fenced"
        );
        assert_eq!(
            store
                .latest("deferred-accounting")
                .await
                .unwrap()
                .unwrap()
                .revision,
            checkpoint.revision
        );
        assert!(agent.framework().close().await.is_success());
    }
}
