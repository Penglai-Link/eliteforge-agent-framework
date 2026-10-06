use async_trait::async_trait;
use eliteforge_agent_context::*;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_storage_local::{
    SqliteOptions, SqliteStateStore, SqliteSummaryStore, SummaryStoreOptions,
};
use eliteforge_agent_test_support::*;
use serde_json::json;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct Fixture {
    requests: Arc<Mutex<Vec<(String, ChatRequest)>>>,
    mode: &'static str,
}
#[async_trait]
impl ChatModel for Fixture {
    async fn generate(
        &self,
        id: &str,
        request: ChatRequest,
        stream: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        self.requests
            .lock()
            .unwrap()
            .push((id.into(), request.clone()));
        let content = if id == "summary" {
            stream.text_delta("summary text must remain private").await;
            if self.mode == "failure" {
                return Err(ModelError::Fatal("fixture unavailable".into()));
            }
            let summary = match self.mode {
                "oversized" => "x".repeat(40_000),
                "empty" => "".into(),
                _ => {
                    let data = &request.messages[1].content.0[0];
                    match data {
                        ContentBlock::Json { value } => format!(
                            "Retained {} historical turns.",
                            value["history"].as_array().unwrap().len()
                        ),
                        _ => panic!("history must be typed data"),
                    }
                }
            };
            Content(vec![ContentBlock::Json {
                value: json!({"summary":summary}),
            }])
        } else {
            assert!(request.messages.iter().any(|m| m.role == Role::User
                && m.content.text().contains("Earlier conversation summary")));
            "answer".into()
        };
        let mut response = ChatResponse {
            content,
            usage: Usage {
                input_tokens: 9,
                output_tokens: 3,
            },
            ..Default::default()
        };
        if id == "summary" && self.mode == "structured_only" {
            response.structured =
                Some(json!({"summary":"Historical facts from structured output"}));
            response.content = Content::default();
        }
        if id == "summary" && self.mode == "structured_wrong" {
            response.structured = Some(json!({"wrong":true}));
            response.content = Content::default();
        }
        if id == "summary" && self.mode == "structured_mismatch" {
            response.structured = Some(json!({"summary":"different payload"}));
        }
        Ok(response)
    }
}
fn fixture(mode: &'static str) -> Fixture {
    Fixture {
        requests: Arc::default(),
        mode,
    }
}
fn builder(provider: Fixture) -> TestBuilder {
    TestBuilder::default()
        .plugin(ModelConnectionPlugin::new(ModelConnection::new(
            "shared",
            Arc::new(provider),
            vec![
                ModelDescriptor::new("main"),
                ModelDescriptor::new("summary"),
            ],
        )))
        .plugin(ModelStrategyPlugin(Arc::new(FixedModelStrategy(
            ModelRef::new("shared", "main"),
        ))))
        .plugin(ContextReducerPlugin::new(
            ModelCompressor::new(
                ModelRef::new("shared", "summary"),
                CompressionOptions::default(),
            )
            .unwrap(),
        ))
}
fn history() -> Vec<Message> {
    (0..12)
        .flat_map(|i| {
            [
                Message::user(format!("topic-{i}: {}", "context ".repeat(25))),
                Message::assistant(format!("decision-{i}")),
            ]
        })
        .collect()
}
fn request() -> RunRequest {
    let mut request = RunRequest::new("s", "latest user input");
    request.limits.max_context_messages = 8;
    request
}

#[tokio::test]
async fn react_compression_uses_shared_invoker_without_rewriting_source_or_streaming_summary() {
    let original = history();
    let memory = TestConversationStore::default();
    memory
        .open("s")
        .await
        .unwrap()
        .append(&original)
        .await
        .unwrap();
    let provider = fixture("success");
    let agent = builder(provider.clone())
        .plugin(ConversationStorePlugin::new(memory.clone()))
        .build()
        .await
        .unwrap();
    let mut handle = agent.start(request()).unwrap();
    let mut events = agent.events(&mut handle).unwrap();
    let result = agent.wait(handle).await.unwrap();
    assert_eq!(result.output, "answer");
    {
        let calls = provider.requests.lock().unwrap();
        assert_eq!(
            calls.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(),
            ["summary", "main"]
        );
        assert_eq!(memory.snapshot("s")[..original.len()], original);
        assert_eq!(memory.snapshot("s").len(), original.len() + 2);
        while let Ok(event) = events.try_recv() {
            assert!(
                !serde_json::to_string(&event)
                    .unwrap()
                    .contains("summary text must remain private")
            );
        }
    }
    agent.framework().close().await;
}

#[tokio::test]
async fn workflow_projection_compresses_while_sqlite_keeps_complete_history() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        SqliteStateStore::open(dir.path().join("state.db"), SqliteOptions::default())
            .await
            .unwrap(),
    );
    let mut old = Checkpoint::new(RunId::new_v4(), "s", "previous", 1, json!({}));
    let original = history();
    old.transcript = original.clone();
    let old = store.create(old, None).await.unwrap();
    let lease = store
        .claim(
            old.run_id,
            RunId::new_v4(),
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
    let mut terminal = old.clone();
    terminal.status = ExecutionStatus::Completed;
    store.commit(&lease, old.revision, terminal).await.unwrap();
    store.release(&lease).await.unwrap();
    let agent = builder(fixture("success"))
        .plugin(StateStorePlugin(store.clone()))
        .plugin(AgentLoopPlugin::new(
            WorkflowLoop::new(WorkflowDefinition {
                id: "workflow".into(),
                version: 1,
                stages: vec![WorkflowStage::new("answer")],
            })
            .unwrap(),
        ))
        .build()
        .await
        .unwrap();
    assert_eq!(agent.run(request()).await.unwrap().output, "answer");
    let saved = store.latest("s").await.unwrap().unwrap();
    assert_eq!(saved.transcript[..original.len()], original);
    assert_eq!(saved.transcript.len(), original.len() + 2);
    assert!(
        saved.state["budget_usage"]["input_tokens"]
            .as_u64()
            .unwrap()
            >= 18
    );
    agent.framework().close().await;
    store.close().await.unwrap();
}

#[tokio::test]
async fn unusable_summaries_fail_without_silently_truncating_or_calling_main_model() {
    for mode in [
        "failure",
        "oversized",
        "empty",
        "structured_wrong",
        "structured_mismatch",
    ] {
        let original = history();
        let memory = TestConversationStore::default();
        memory
            .open("s")
            .await
            .unwrap()
            .append(&original)
            .await
            .unwrap();
        let provider = fixture(mode);
        let agent = builder(provider.clone())
            .plugin(ConversationStorePlugin::new(memory.clone()))
            .build()
            .await
            .unwrap();
        assert!(agent.run(request()).await.is_err(), "{mode}");
        assert_eq!(memory.snapshot("s")[..original.len()], original);
        assert!(
            provider
                .requests
                .lock()
                .unwrap()
                .iter()
                .all(|(id, _)| id == "summary")
        );
        agent.framework().close().await;
    }
}

struct PreserveLatest {
    messages: Vec<Message>,
}
#[async_trait]
impl AgentLoop for PreserveLatest {
    async fn run(
        &self,
        request: &RunRequest,
        context: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let reducer = ModelCompressor::new(
            ModelRef::new("shared", "summary"),
            CompressionOptions::default(),
        )?;
        let projected = reducer
            .reduce(&ContextReductionInput {
                block: &ContextBlock {
                    id: HISTORY_BLOCK.into(),
                    messages: self.messages.clone(),
                    trust: ContextTrust::Data,
                    source_version: None,
                },
                budget: ContextBudget {
                    max_messages: 6,
                    max_tokens: 10_000,
                },
                run: context,
                models: services.models.as_deref().unwrap(),
                limits: &request.limits,
                history: &ConversationSnapshot::new(
                    ConversationKey::new(&request.session_id),
                    "fixture",
                    self.messages.clone(),
                ),
                model: &ModelRef::new("shared", "summary"),
                estimator: &ConservativeTokenEstimator,
                overflow_recoveries: 0,
                compression: None,
            })
            .await?;
        let latest = self
            .messages
            .iter()
            .rposition(|m| m.role == Role::User)
            .unwrap();
        assert_eq!(
            projected[projected.len() - (self.messages.len() - latest)..],
            self.messages[latest..]
        );
        Ok(RunResult {
            output: String::new(),
            content: Content::default(),
            structured: None,
            interactions: vec![],
            status: RunStatus::Completed,
            usage: context.usage(),
            steps: 0,
            tool_calls: 0,
            external_session: None,
        })
    }
}
#[tokio::test]
async fn reduction_preserves_newest_signed_tool_exchange_exactly() {
    let mut messages = history();
    messages.push(Message::user("current question"));
    let call = ToolCall::new("signed-call", "inspect", json!({"path":"a"}));
    let mut assistant = Message::assistant("thinking");
    assistant.tool_calls = vec![call];
    assistant.model_context = Some(ModelMessageContext {
        protocol: "custom".into(),
        model: ModelRef::new("shared", "main"),
        format: "opaque-signed".into(),
        data: json!({"signature":"exact-signature"}),
    });
    messages.push(assistant);
    let mut result = Message::tool("signed-call", "observed");
    result.tool_result = Some(ToolResultMetadata {
        is_error: true,
        structured: Some(json!({"code":4})),
        result_reference: None,
    });
    messages.push(result);
    let agent = builder(fixture("success"))
        .plugin(AgentLoopPlugin::new(PreserveLatest { messages }))
        .build()
        .await
        .unwrap();
    let result = agent.run(RunRequest::new("s", "unused")).await.unwrap();
    assert_eq!(result.usage.input_tokens, 9);
    agent.framework().close().await;
}

struct CachedProjection {
    messages: Vec<Message>,
    store: Arc<dyn SummaryStore>,
    generation: &'static str,
    branch: &'static str,
    policy: &'static str,
}
#[async_trait]
impl AgentLoop for CachedProjection {
    async fn run(
        &self,
        request: &RunRequest,
        context: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let reducer = ModelCompressor::new(
            ModelRef::new("shared", "summary"),
            CompressionOptions::default(),
        )?
        .with_store(self.store.clone(), self.policy)?;
        let snapshot = ConversationSnapshot::new(
            ConversationKey::new(&request.session_id).branch(self.branch),
            self.generation,
            self.messages.clone(),
        );
        let block = ContextBlock {
            id: HISTORY_BLOCK.into(),
            messages: self.messages.clone(),
            trust: ContextTrust::Data,
            source_version: Some(snapshot.revision.to_string()),
        };
        let projected = reducer
            .reduce(&ContextReductionInput {
                block: &block,
                budget: ContextBudget {
                    max_messages: 6,
                    max_tokens: 10_000,
                },
                model: &ModelRef::new("shared", "main"),
                estimator: &ConservativeTokenEstimator,
                history: &snapshot,
                run: context,
                models: services.models.as_deref().unwrap(),
                limits: &request.limits,
                overflow_recoveries: 0,
                compression: None,
            })
            .await?;
        assert!(projected.iter().any(|m| m.role == Role::User
            && m.content.text().starts_with("Earlier conversation summary")));
        assert!(
            projected.ends_with(
                &self.messages[self
                    .messages
                    .iter()
                    .rposition(|m| m.role == Role::User)
                    .unwrap()..]
            )
        );
        Ok(RunResult {
            output: "projected".into(),
            content: Content::default(),
            structured: None,
            interactions: vec![],
            status: RunStatus::Completed,
            usage: context.usage(),
            steps: 0,
            tool_calls: 0,
            external_session: None,
        })
    }
}
async fn cached_projection(provider: Fixture, projection: CachedProjection) {
    let agent = builder(provider)
        .plugin(AgentLoopPlugin::new(projection))
        .build()
        .await
        .unwrap();
    agent
        .run(RunRequest::new("summary-chat", "unused"))
        .await
        .unwrap();
    agent.framework().close().await;
}

#[tokio::test]
async fn persisted_summary_survives_reopen_and_only_summarizes_new_complete_turns() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("summaries.db");
    let store = Arc::new(
        SqliteSummaryStore::open(&path, SummaryStoreOptions::default())
            .await
            .unwrap(),
    );
    let provider = fixture("success");
    let mut messages = history();
    messages[1].structured = Some(json!({"artifact_id": "result-42"}));
    messages[1].model_context = Some(ModelMessageContext {
        protocol: "custom".into(),
        model: ModelRef::new("shared", "main"),
        format: "opaque".into(),
        data: json!({"secret-signature": "private"}),
    });
    messages.push(Message::user("current question"));
    cached_projection(
        provider.clone(),
        CachedProjection {
            messages: messages.clone(),
            store: store.clone(),
            generation: "g1",
            branch: "main",
            policy: "v1",
        },
    )
    .await;
    {
        let calls = provider.requests.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let data = serde_json::to_value(&calls[0].1.messages).unwrap();
        let data = data.to_string();
        assert!(data.contains("result-42"));
        assert!(!data.contains("secret-signature"));
    }
    store.close().await.unwrap();
    let store = Arc::new(
        SqliteSummaryStore::open(&path, SummaryStoreOptions::default())
            .await
            .unwrap(),
    );
    cached_projection(
        provider.clone(),
        CachedProjection {
            messages: messages.clone(),
            store: store.clone(),
            generation: "g1",
            branch: "main",
            policy: "v1",
        },
    )
    .await;
    assert_eq!(
        provider.requests.lock().unwrap().len(),
        1,
        "same prefix reuses persisted summary without another model call"
    );
    messages.push(Message::assistant("resolved current question"));
    messages.push(Message::user("next question"));
    cached_projection(
        provider.clone(),
        CachedProjection {
            messages: messages.clone(),
            store: store.clone(),
            generation: "g1",
            branch: "main",
            policy: "v1",
        },
    )
    .await;
    {
        let calls = provider.requests.lock().unwrap();
        assert_eq!(calls.len(), 2);
        let ContentBlock::Json { value } = &calls[1].1.messages[1].content.0[0] else {
            panic!()
        };
        assert_eq!(value["history"].as_array().unwrap().len(), 1);
        assert!(!value["previous_summary"].as_str().unwrap().is_empty());
    }
    messages[1].structured = Some(json!({"artifact_id": "corrected-result"}));
    cached_projection(
        provider.clone(),
        CachedProjection {
            messages,
            store: store.clone(),
            generation: "g1",
            branch: "main",
            policy: "v1",
        },
    )
    .await;
    {
        let calls = provider.requests.lock().unwrap();
        let ContentBlock::Json { value } = &calls[2].1.messages[1].content.0[0] else {
            panic!()
        };
        assert_eq!(value["previous_summary"], "");
        assert_eq!(
            value["history"].as_array().unwrap().len(),
            13,
            "changed source digest invalidates old summary"
        );
    }
    store.close().await.unwrap();
}

#[tokio::test]
async fn summary_identity_separates_branch_generation_and_policy_version() {
    let provider = fixture("success");
    let summary_dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn SummaryStore> = Arc::new(
        SqliteSummaryStore::open(summary_dir.path().join("summary.db"), Default::default())
            .await
            .unwrap(),
    );
    let mut messages = history();
    messages.push(Message::user("latest"));
    for (generation, branch, policy) in [
        ("g1", "main", "v1"),
        ("g1", "node-b", "v1"),
        ("g2", "main", "v1"),
        ("g1", "main", "v2"),
    ] {
        cached_projection(
            provider.clone(),
            CachedProjection {
                messages: messages.clone(),
                store: store.clone(),
                generation,
                branch,
                policy,
            },
        )
        .await;
    }
    assert_eq!(provider.requests.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn auxiliary_failure_keeps_typed_model_error_instead_of_flattening_it() {
    let store = TestConversationStore::default();
    store
        .open("s")
        .await
        .unwrap()
        .append(&history())
        .await
        .unwrap();
    let agent = builder(fixture("failure"))
        .plugin(ConversationStorePlugin::new(store))
        .build()
        .await
        .unwrap();
    let error = agent.run(request()).await.unwrap_err();
    assert!(matches!(
        error.primary(),
        AgentError::Model(ModelError::Fatal(_))
    ));
    assert_eq!(error.report(&OmitErrorMessages).kind, "model_fatal");
    agent.framework().close().await;
}

struct DoubleEstimate;
#[async_trait]
impl TokenEstimator for DoubleEstimate {
    async fn message(&self, _: &ModelRef, message: &Message) -> Result<usize, ModelError> {
        Ok(estimate_message_tokens(message).saturating_mul(2))
    }
}
struct SmallSummaryWindow;
#[async_trait]
impl AgentLoop for SmallSummaryWindow {
    async fn run(
        &self,
        request: &RunRequest,
        context: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let compressor = ModelCompressor::new(
            ModelRef::new("shared", "summary"),
            CompressionOptions {
                max_input_tokens: 5000,
                max_summary_bytes: 100,
                max_model_calls: 16,
            },
        )?;
        let mut messages = history();
        messages.push(Message::user("latest"));
        compressor
            .compress(
                &messages,
                ContextBudget {
                    max_messages: 6,
                    max_tokens: 10000,
                },
                context,
                services.models.as_deref().unwrap(),
                &request.limits,
            )
            .await?;
        Ok(RunResult {
            output: "done".into(),
            content: Content::default(),
            structured: None,
            interactions: vec![],
            status: RunStatus::Completed,
            usage: context.usage(),
            steps: 0,
            tool_calls: 0,
            external_session: None,
        })
    }
}
#[tokio::test]
async fn summary_packing_uses_its_connection_estimator_and_output_reservation() {
    let provider = fixture("success");
    let mut descriptor = ModelDescriptor::new("summary");
    descriptor.context_window_tokens = Some(5000);
    let agent = TestBuilder::default()
        .plugin(ModelConnectionPlugin::new(
            ModelConnection::new("shared", Arc::new(provider.clone()), vec![descriptor])
                .with_token_estimator(Arc::new(DoubleEstimate)),
        ))
        .plugin(AgentLoopPlugin::new(SmallSummaryWindow))
        .build()
        .await
        .unwrap();
    agent.run(RunRequest::new("small", "unused")).await.unwrap();
    {
        let calls = provider.requests.lock().unwrap().clone();
        assert!(
            calls.len() > 1,
            "summary-model capacity requires multiple bounded calls"
        );
        for (_, request) in calls.iter() {
            assert!(
                DoubleEstimate
                    .request(&ModelRef::new("shared", "summary"), request)
                    .await
                    .unwrap()
                    + request.options.max_output_tokens.unwrap() as usize
                    <= 5000
            );
        }
    }
    agent.framework().close().await;
}

#[tokio::test]
async fn summary_provider_can_return_only_the_structured_response_field() {
    let memory = TestConversationStore::default();
    memory
        .open("s")
        .await
        .unwrap()
        .append(&history())
        .await
        .unwrap();
    let agent = builder(fixture("structured_only"))
        .plugin(ConversationStorePlugin::new(memory))
        .build()
        .await
        .unwrap();
    assert_eq!(agent.run(request()).await.unwrap().output, "answer");
    agent.framework().close().await;
}
