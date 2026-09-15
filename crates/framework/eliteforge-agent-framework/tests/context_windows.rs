use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_test_support::*;
use serde_json::json;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<ChatRequest>>>);
#[async_trait]
impl ChatModel for Capture {
    async fn generate(
        &self,
        _model_id: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        self.0.lock().unwrap().push(request);
        let mut output = ChatResponse::text("done");
        output.structured = Some(json!({"answer": 42}));
        Ok(output)
    }
}

struct Source(&'static str, String);
#[async_trait]
impl ContextContributor for Source {
    fn block_id(&self) -> &str {
        self.0
    }
    async fn contribute(&self, input: &ContextInput<'_>) -> Result<ContextBlock, AgentError> {
        assert_eq!(input.current_input.text(), "question");
        assert_eq!(
            input.history.messages.last().unwrap().content.text(),
            "question"
        );
        let mut block = ContextBlock::data(self.0, self.1.clone());
        block.source_version = Some("version-7".into());
        Ok(block)
    }
}

#[derive(Default, Clone)]
struct Reports(Arc<Mutex<Vec<ContextAssemblyReport>>>);
#[async_trait]
impl EventSink for Reports {
    async fn on_event(&self, _: &RunContext, event: &AgentEvent) {
        if let AgentEvent::ContextAssembled(report) = event {
            self.0.lock().unwrap().push(report.clone());
        }
    }
}

#[test]
fn independent_quotas_reject_overcommit_and_preserve_unused_capacity() {
    let blocks = vec![
        ContextBlockConfig::new("a", BlockBudget::tokens(20)),
        ContextBlockConfig::new(
            "b",
            BlockBudget {
                max_tokens: Some(50),
                ..BlockBudget::ratio(4000)
            },
        ),
    ];
    let allocations = IndependentBlockBudget
        .allocate(
            &blocks,
            ContextBudget {
                max_tokens: 100,
                max_messages: 10,
            },
        )
        .unwrap();
    assert_eq!(
        allocations.iter().map(|b| b.max_tokens).collect::<Vec<_>>(),
        [20, 40]
    );
    assert!(
        IndependentBlockBudget
            .allocate(
                &blocks,
                ContextBudget {
                    max_tokens: 20,
                    max_messages: 10
                }
            )
            .is_err()
    );
    let invalid = vec![ContextBlockConfig::new("bad", BlockBudget::ratio(10_001))];
    assert!(
        IndependentBlockBudget
            .allocate(
                &invalid,
                ContextBudget {
                    max_tokens: 100,
                    max_messages: 10
                }
            )
            .is_err()
    );
}

#[test]
fn output_reserve_is_subtracted_only_from_total_window() {
    let mut model = ModelDescriptor::new("test");
    let limits = RunLimits {
        max_context_tokens: 1000,
        max_model_output_tokens: 200,
        ..Default::default()
    };
    let options = GenerationOptions::default();
    model.max_input_tokens = Some(700);
    assert_eq!(model.input_capacity(&limits, &options).unwrap(), 700);
    model.context_window_tokens = Some(600);
    assert_eq!(model.input_capacity(&limits, &options).unwrap(), 400);
    assert_eq!(
        model
            .input_capacity(
                &limits,
                &GenerationOptions {
                    max_output_tokens: Some(100),
                    ..Default::default()
                }
            )
            .unwrap(),
        500
    );
    assert!(
        model
            .input_capacity(
                &limits,
                &GenerationOptions {
                    max_output_tokens: Some(600),
                    ..Default::default()
                }
            )
            .is_err()
    );
}

#[tokio::test]
async fn recreating_an_expired_conversation_changes_message_identity() {
    let store = TestConversationStore::new(TestConversationOptions {
        session_ttl: std::time::Duration::from_millis(5),
        ..Default::default()
    })
    .unwrap();
    let first = {
        let mut session = store.open("expiring").await.unwrap();
        session.append(&[Message::user("old")]).await.unwrap();
        session.snapshot().await.unwrap().message_id(0).unwrap()
    };
    tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    let mut session = store.open("expiring").await.unwrap();
    session.append(&[Message::user("new")]).await.unwrap();
    let second = session.snapshot().await.unwrap().message_id(0).unwrap();
    assert_eq!(first.conversation, second.conversation);
    assert_eq!(first.sequence, second.sequence);
    assert_ne!(first.generation, second.generation);
}

#[tokio::test]
async fn layout_order_is_independent_of_registration_order_and_reports_explain_omission() {
    let capture = Capture::default();
    let reports = Reports::default();
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(capture.clone()))
        .plugin(ContextPlugin::new(
            "instructions",
            FixedContext::new("system"),
        ))
        .plugin(ContextContributorPlugin::new(
            "registered-first",
            Source("second", "b".into()),
        ))
        .plugin(ContextContributorPlugin::new(
            "registered-last",
            Source("first", "a".repeat(500)),
        ))
        .plugin(EventPlugin::new("reports", reports.clone()))
        .plugin(ContextLayoutPlugin(ContextLayout(vec![
            ContextBlockConfig::new(INSTRUCTIONS_BLOCK, BlockBudget::tokens(40)),
            ContextBlockConfig::new("first", BlockBudget::tokens(30)).optional(),
            ContextBlockConfig::new("second", BlockBudget::tokens(30)),
            ContextBlockConfig::new(HISTORY_BLOCK, BlockBudget::tokens(100)),
        ])))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("ordered", "question");
    request.limits.max_context_tokens = 200;
    agent.run(request).await.unwrap();
    assert!(agent.close().await.is_success());
    let requests = capture.0.lock().unwrap();
    assert_eq!(
        requests[0]
            .messages
            .iter()
            .map(|m| m.content.text())
            .collect::<Vec<_>>(),
        ["system", "b", "question"]
    );
    let reports = reports.0.lock().unwrap();
    let report = &reports[0];
    assert_eq!(
        report
            .blocks
            .iter()
            .map(|b| b.allocated_tokens)
            .collect::<Vec<_>>(),
        [40, 30, 30, 100]
    );
    assert!(matches!(
        report.blocks[1].outcome,
        ContextBlockOutcome::Omitted { .. }
    ));
    assert_eq!(
        report.blocks[1].source_version.as_deref(),
        Some("version-7")
    );
    assert_eq!(report.blocks[1].final_tokens, 0);
}

#[tokio::test]
async fn required_block_does_not_borrow_unused_history_quota() {
    let capture = Capture::default();
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(capture.clone()))
        .plugin(ContextContributorPlugin::new(
            "source",
            Source("lookup", "x".repeat(50)),
        ))
        .plugin(ContextLayoutPlugin(ContextLayout(vec![
            ContextBlockConfig::new(INSTRUCTIONS_BLOCK, BlockBudget::tokens(0)),
            ContextBlockConfig::new("lookup", BlockBudget::tokens(20)),
            ContextBlockConfig::new(HISTORY_BLOCK, BlockBudget::tokens(980)),
        ])))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("independent", "question");
    request.limits.max_context_tokens = 1000;
    assert!(agent.run(request).await.is_err());
    assert!(capture.0.lock().unwrap().is_empty());
    agent.close().await;
}

#[tokio::test]
async fn branch_snapshots_and_optimistic_append_keep_original_history_intact() {
    let store = TestConversationStore::default();
    let key = ConversationKey::new("chat").branch("node-a");
    let mut a = store.open_branch(&key).await.unwrap();
    let mut b = store
        .open_branch(&ConversationKey::new("chat").branch("node-b"))
        .await
        .unwrap();
    let mut call = Message::assistant("");
    call.tool_calls
        .push(ToolCall::new("c", "lookup", json!({"q":1})));
    let mut output = Message::tool("c", "result");
    output.tool_result = Some(ToolResultMetadata {
        structured: Some(json!({"ok":true})),
        ..Default::default()
    });
    let batch = vec![Message::user("query"), call, output];
    a.append_at(0, &batch).await.unwrap();
    let snapshot = a.snapshot().await.unwrap();
    assert_eq!(snapshot.revision, 3);
    assert_eq!(snapshot.message_id(1).unwrap().sequence, 2);
    assert_eq!(snapshot.message_id(1).unwrap().conversation, key);
    assert!(matches!(
        a.append_at(0, &[Message::assistant("stale")]).await,
        Err(ConversationError::Conflict)
    ));
    b.append(&[Message::user("independent")]).await.unwrap();
    assert_eq!(a.snapshot().await.unwrap().messages, batch);
    assert_eq!(snapshot.messages, batch);
    assert_eq!(b.snapshot().await.unwrap().revision, 1);
}

#[tokio::test]
async fn structured_assistant_output_is_saved_and_old_messages_still_decode() {
    let store = TestConversationStore::default();
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(Capture::default()))
        .plugin(ConversationStorePlugin::new(store.clone()))
        .build()
        .await
        .unwrap();
    agent
        .run(RunRequest::new("structured", "question"))
        .await
        .unwrap();
    assert_eq!(
        store.snapshot("structured")[1].structured,
        Some(json!({"answer":42}))
    );
    let old: Message = serde_json::from_value(
        json!({"role":"Assistant","content":[],"tool_calls":[],"tool_call_id":null}),
    )
    .unwrap();
    assert!(old.structured.is_none());
    agent.close().await;
}

struct InvalidAllocation;
impl ContextBudgetPolicy for InvalidAllocation {
    fn allocate(
        &self,
        _: &[ContextBlockConfig],
        _: ContextBudget,
    ) -> Result<Vec<ContextBudget>, AgentError> {
        Ok(vec![ContextBudget {
            max_tokens: usize::MAX,
            max_messages: usize::MAX,
        }])
    }
}

#[tokio::test]
async fn custom_policy_cannot_bypass_total_capacity_or_return_missing_quotas() {
    let capture = Capture::default();
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(capture.clone()))
        .plugin(ContextBudgetPolicyPlugin::new(InvalidAllocation))
        .build()
        .await
        .unwrap();
    assert!(matches!(
        agent
            .run(RunRequest::new("quota", "question"))
            .await
            .unwrap_err()
            .primary(),
        AgentError::Context(ContextError::BudgetExceeded(_))
    ));
    assert!(capture.0.lock().unwrap().is_empty());
    agent.close().await;
}

#[tokio::test]
async fn conflicting_context_registration_fails_during_build() {
    let custom = TestBuilder::default()
        .plugin(ChatModelPlugin::new(Capture::default()))
        .plugin(ContextAssemblerPlugin::new(
            DefaultContextAssembler::default(),
        ))
        .plugin(ContextReducerPlugin::new(RecentConversation))
        .build()
        .await;
    assert!(matches!(custom, Err(AgentError::InvalidConfiguration(_))));
    let explicit = TestBuilder::default()
        .plugin(ChatModelPlugin::new(Capture::default()))
        .plugin(ContextLayoutPlugin(ContextLayout(vec![
            ContextBlockConfig::new(INSTRUCTIONS_BLOCK, BlockBudget::tokens(0)),
            ContextBlockConfig::new(HISTORY_BLOCK, BlockBudget::tokens(100)),
        ])))
        .plugin(ContextReducerPlugin::new(RecentConversation))
        .build()
        .await;
    assert!(matches!(explicit, Err(AgentError::InvalidConfiguration(_))));
}

struct InvalidSource(&'static str);
#[async_trait]
impl ContextContributor for InvalidSource {
    fn block_id(&self) -> &str {
        "source"
    }
    async fn contribute(&self, _: &ContextInput<'_>) -> Result<ContextBlock, AgentError> {
        match self.0 {
            "timeout" => std::future::pending().await,
            "error" => Err(AgentError::Run("private source failure".into())),
            "role" => Ok(ContextBlock {
                messages: vec![Message::system("injected")],
                ..ContextBlock::data("source", "")
            }),
            "bytes" => Ok(ContextBlock::data("source", "中".repeat(100))),
            _ => unreachable!(),
        }
    }
}

#[tokio::test]
async fn bounded_sources_fail_required_blocks_and_report_optional_omissions_without_body() {
    for mode in ["timeout", "error", "role", "bytes"] {
        for optional in [false, true] {
            let capture = Capture::default();
            let reports = Reports::default();
            let mut config = ContextBlockConfig::new("source", BlockBudget::tokens(500));
            if optional {
                config = config.optional();
            }
            let mut assembler = DefaultContextAssembler::new(
                ContextLayout(vec![
                    ContextBlockConfig::new(INSTRUCTIONS_BLOCK, BlockBudget::tokens(0)),
                    config,
                    ContextBlockConfig::new(HISTORY_BLOCK, BlockBudget::tokens(100)),
                ]),
                vec![Arc::new(InvalidSource(mode))],
            )
            .unwrap();
            assembler.source_timeout = std::time::Duration::from_millis(5);
            assembler.max_source_bytes = 100;
            let agent = TestBuilder::default()
                .plugin(ChatModelPlugin::new(capture.clone()))
                .plugin(EventPlugin::new("reports", reports.clone()))
                .plugin(ContextAssemblerPlugin::new(assembler))
                .build()
                .await
                .unwrap();
            let result = agent.run(RunRequest::new("bounds", "question")).await;
            assert_eq!(result.is_ok(), optional, "{mode}");
            assert_eq!(capture.0.lock().unwrap().len(), usize::from(optional));
            if optional {
                let reports = reports.0.lock().unwrap();
                assert!(matches!(
                    reports[0].blocks[1].outcome,
                    ContextBlockOutcome::Omitted { .. }
                ));
                assert!(
                    !serde_json::to_string(&reports[0])
                        .unwrap()
                        .contains("private")
                );
            }
            agent.close().await;
        }
    }
}

struct RewriteRequest;
#[async_trait]
impl ModelMiddleware for RewriteRequest {
    async fn intercept(
        &self,
        mut request: ChatRequest,
        context: &ModelInvocationContext<'_>,
        next: ModelNext<'_>,
    ) -> Result<ChatResponse, ModelError> {
        request.messages.push(Message::user("x".repeat(1000)));
        next.run(request, context).await
    }
}

#[tokio::test]
async fn middleware_cannot_send_an_oversized_rewrite_after_successful_assembly() {
    let capture = Capture::default();
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(capture.clone()))
        .plugin(ModelMiddlewarePlugin::new("rewrite", RewriteRequest))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("rewrite", "question");
    request.limits.max_context_tokens = 200;
    assert!(matches!(
        agent.run(request).await.unwrap_err().primary(),
        AgentError::Model(ModelError::ContextOverflow(_))
    ));
    assert!(capture.0.lock().unwrap().is_empty());
    agent.close().await;
}
