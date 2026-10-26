use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_memory::*;
use eliteforge_agent_official_plugin_storage_local::{
    SqliteConversationOptions, SqliteConversationStore, SqliteLongTermMemory, SqliteSummaryStore,
    SummaryStoreOptions,
};
use eliteforge_agent_test_support::*;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

fn draft(content: &str) -> MemoryWrite {
    MemoryWrite {
        id: "preference".into(),
        content: content.into(),
        source: MemorySource {
            id: "explicit-user-input".into(),
            version: None,
        },
        expires_at_ms: None,
    }
}
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
        Ok(ChatResponse::text("done"))
    }
}
#[tokio::test]
async fn retrieval_block_is_bounded_and_deleted_or_corrected_entries_do_not_reappear_from_cache() {
    let memory_dir = tempfile::tempdir().unwrap();
    let memory = Arc::new(
        SqliteLongTermMemory::open(memory_dir.path().join("memory.db"), Default::default())
            .await
            .unwrap(),
    );
    let initial = memory
        .save("authorized", &draft("Rust original preference"), None)
        .await
        .unwrap();
    memory
        .save("other", &draft("Rust private outside namespace"), None)
        .await
        .unwrap();
    let capture = Capture::default();
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(capture.clone()))
        .plugin(LongTermMemoryPlugin::from_arc(memory.clone()))
        .plugin(ContextContributorPlugin::new(
            "memories",
            MemoryContributor::new("memory", "authorized", memory.clone()).unwrap(),
        ))
        .plugin(ContextLayoutPlugin(ContextLayout(vec![
            ContextBlockConfig::new(INSTRUCTIONS_BLOCK, BlockBudget::tokens(0)),
            ContextBlockConfig::new("memory", BlockBudget::tokens(1000)),
            ContextBlockConfig::new(HISTORY_BLOCK, BlockBudget::tokens(1000)),
        ])))
        .build()
        .await
        .unwrap();
    agent.run(RunRequest::new("chat-1", "Rust")).await.unwrap();
    let updated = memory
        .save(
            "authorized",
            &draft("Rust corrected preference"),
            Some(&initial.version),
        )
        .await
        .unwrap();
    agent.run(RunRequest::new("chat-2", "Rust")).await.unwrap();
    memory
        .delete("authorized", "preference", &updated.version)
        .await
        .unwrap();
    agent.run(RunRequest::new("chat-3", "Rust")).await.unwrap();
    {
        let calls = capture.0.lock().unwrap();
        let first = serde_json::to_string(&calls[0].messages).unwrap();
        let second = serde_json::to_string(&calls[1].messages).unwrap();
        let third = serde_json::to_string(&calls[2].messages).unwrap();
        assert!(first.contains("original preference"));
        assert!(second.contains("corrected preference"));
        assert!(!second.contains("original preference"));
        assert!(!third.contains("preference"));
        assert!(!first.contains("private outside"));
        assert_eq!(calls[0].messages[0].role, Role::User);
        assert!(first.contains(&initial.version));
        assert!(first.contains("explicit-user-input"));
    }
    agent.framework().close().await;
}
struct ToolChecks(Arc<dyn LongTermMemory>);
#[async_trait]
impl AgentLoop for ToolChecks {
    fn requires_model(&self) -> bool {
        false
    }
    async fn run(
        &self,
        _: &RunRequest,
        context: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        assert!(services.long_term_memory.is_some());
        let tools = MemoryTools::new(self.0.clone(), "allowed")?
            .allow_writes()
            .bindings();
        let execute = |name: &str| {
            tools
                .iter()
                .find(|tool| tool.descriptor.call_name == name)
                .unwrap()
                .executor
                .clone()
        };
        let saved = execute("memory_save").call(json!({"id":"preference","content":"Rust is preferred","source":{"id":"explicit-user-input"}}), context).await.unwrap();
        assert!(!saved.is_error);
        let version = saved.structured.unwrap()["entry"]["version"]
            .as_str()
            .unwrap()
            .to_owned();
        let duplicate = execute("memory_save")
            .call(
                json!({"id":"preference","content":"stale","source":{"id":"explicit-user-input"}}),
                context,
            )
            .await
            .unwrap();
        assert!(duplicate.is_error);
        assert_eq!(
            duplicate.structured.unwrap()["error"]["code"],
            "memory_conflict"
        );
        let found = execute("memory_search")
            .call(json!({"query":"Rust"}), context)
            .await
            .unwrap();
        assert_eq!(
            found.structured.unwrap()["entries"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let escaped = execute("memory_get")
            .call(json!({"id":"preference","namespace":"other"}), context)
            .await
            .unwrap();
        assert!(
            escaped.is_error,
            "model cannot replace the host-bound namespace"
        );
        let deleted = execute("memory_delete")
            .call(
                json!({"id":"preference","expected_version":version}),
                context,
            )
            .await
            .unwrap();
        assert!(!deleted.is_error);
        let missing = execute("memory_get")
            .call(json!({"id":"preference"}), context)
            .await
            .unwrap();
        assert_eq!(
            missing.structured.unwrap()["error"]["code"],
            "memory_not_found"
        );
        Ok(RunResult {
            output: "checked".into(),
            content: Content::default(),
            structured: None,
            interactions: vec![],
            status: RunStatus::Completed,
            usage: Usage::default(),
            steps: 0,
            tool_calls: 0,
            external_session: None,
        })
    }
}
#[tokio::test]
async fn explicit_memory_tools_use_fixed_scope_and_report_version_errors() {
    let memory_dir = tempfile::tempdir().unwrap();
    let memory = Arc::new(
        SqliteLongTermMemory::open(memory_dir.path().join("memory.db"), Default::default())
            .await
            .unwrap(),
    );
    let readonly = MemoryTools::new(memory.clone(), "allowed")
        .unwrap()
        .bindings();
    assert_eq!(
        readonly
            .iter()
            .map(|b| b.descriptor.call_name.as_str())
            .collect::<Vec<_>>(),
        ["memory_search", "memory_get"]
    );
    let agent = TestBuilder::default()
        .plugin(AgentLoopPlugin::new(ToolChecks(memory.clone())))
        .plugin(LongTermMemoryPlugin::from_arc(memory))
        .build()
        .await
        .unwrap();
    agent.run(RunRequest::new("tools", "unused")).await.unwrap();
    agent.framework().close().await;
}

#[derive(Clone, Default)]
struct SummaryProvider(Arc<Mutex<Vec<Value>>>, Arc<Mutex<Vec<ChatRequest>>>);
#[async_trait]
impl ChatModel for SummaryProvider {
    async fn generate(
        &self,
        id: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        if id == "summary" {
            let ContentBlock::Json { value } = &request.messages[1].content.0[0] else {
                panic!()
            };
            self.0.lock().unwrap().push(value.clone());
            return Ok(ChatResponse {
                content: Content(vec![ContentBlock::Json {
                    value: json!({"summary":"Prior task facts"}),
                }]),
                ..Default::default()
            });
        }
        self.1.lock().unwrap().push(request);
        Ok(ChatResponse::text("answer"))
    }
}
#[tokio::test]
async fn agent_reopen_reuses_summary_from_persisted_conversation_generation() {
    use eliteforge_agent_context::*;
    let directory = tempfile::tempdir().unwrap();
    let provider = SummaryProvider::default();
    for index in 0..2 {
        let conversations = Arc::new(
            SqliteConversationStore::open(
                directory.path().join("conversation.db"),
                SqliteConversationOptions::default(),
            )
            .await
            .unwrap(),
        );
        let summaries = Arc::new(
            SqliteSummaryStore::open(
                directory.path().join("summary.db"),
                SummaryStoreOptions::default(),
            )
            .await
            .unwrap(),
        );
        if index == 0 {
            let mut session = ConversationStore::open(conversations.as_ref(), "chat")
                .await
                .unwrap();
            let messages: Vec<_> = (0..8)
                .flat_map(|i| {
                    [
                        Message::user(format!("old-{i}")),
                        Message::assistant("decision"),
                    ]
                })
                .collect();
            session.append(&messages).await.unwrap();
            session
                .finish(ConversationOutcome::Completed)
                .await
                .unwrap();
        }
        let agent = TestBuilder::default()
            .plugin(ModelConnectionPlugin::new(ModelConnection::new(
                "connection",
                Arc::new(provider.clone()),
                vec![
                    ModelDescriptor::new("main"),
                    ModelDescriptor::new("summary"),
                ],
            )))
            .plugin(ModelStrategyPlugin(Arc::new(FixedModelStrategy(
                ModelRef::new("connection", "main"),
            ))))
            .plugin(ConversationStorePlugin::from_arc(conversations.clone()))
            .plugin(ContextReducerPlugin::new(
                ModelCompressor::new(
                    ModelRef::new("connection", "summary"),
                    CompressionOptions::default(),
                )
                .unwrap()
                .with_store(summaries.clone(), "v1")
                .unwrap(),
            ))
            .build()
            .await
            .unwrap();
        let mut request = RunRequest::new("chat", "next");
        request.limits.max_context_messages = 6;
        agent.run(request).await.unwrap();
        agent.framework().close().await;
        conversations.close().await.unwrap();
        summaries.close().await.unwrap();
    }
    let calls = provider.0.lock().unwrap();
    // Reopening must reuse the persisted projection while the appended suffix
    // still fits, rather than spending another model call on the same history.
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["history"].as_array().unwrap().len(), 8);
    let main_requests = provider.1.lock().unwrap();
    assert_eq!(main_requests.len(), 2);
    for request in main_requests.iter() {
        assert!(
            request
                .messages
                .iter()
                .any(|message| message.content.text().contains("Prior task facts"))
        );
        assert!(
            request
                .messages
                .iter()
                .any(|message| message.role == Role::User && message.content.text() == "next")
        );
    }
}
