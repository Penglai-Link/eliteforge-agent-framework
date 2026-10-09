use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_storage_local::*;
use eliteforge_agent_test_support::*;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

struct StreamingModel {
    calls: Arc<AtomicUsize>,
    fail: bool,
}
#[async_trait]
impl ChatModel for StreamingModel {
    async fn generate(
        &self,
        _: &str,
        _: ChatRequest,
        stream: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        for _ in 0..4000 {
            stream.reasoning_delta("r").await;
            stream.text_delta("t").await;
        }
        if self.fail {
            return Err(ModelError::Indeterminate("connection lost".into()));
        }
        let mut response = ChatResponse::text("t".repeat(4000));
        if call == 0 {
            response.tool_calls = vec![ToolCall::new(
                "same-provider-id",
                "lookup",
                json!({"key":"value"}),
            )];
        }
        response.public_content = Some(Content(vec![ContentBlock::Text {
            text: "t".repeat(4000),
        }]));
        response.content.0.push(ContentBlock::Json {
            value: json!({"private_wrapper":"PRIVATE-CONTENT-WRAPPER"}),
        });
        response.usage = Usage {
            input_tokens: 10,
            output_tokens: 20,
        };
        response.model_context = Some(ModelMessageContext {
            model: ModelRef::new("fixture", "fixture"),
            protocol: "fixture".into(),
            format: "private".into(),
            data: json!({"secret":"PRIVATE-CONTINUATION"}),
        });
        Ok(response)
    }
}
struct Lookup;
#[async_trait]
impl Tool for Lookup {
    async fn call(&self, _: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("RAW-RESULT"))
    }
}
struct Processed;
#[async_trait]
impl ToolResultProcessor for Processed {
    async fn process(
        &self,
        _: &ToolInvocationContext<'_>,
        _: ToolOutput,
    ) -> Result<ToolOutput, AgentError> {
        Ok(ToolOutput::text("processed result"))
    }
}
async fn build(store: Arc<SqliteStateStore>, fail: bool) -> Agent {
    TestBuilder::default()
        .plugin(store.plugin())
        .plugin(ChatModelPlugin::new(StreamingModel {
            calls: Arc::default(),
            fail,
        }))
        .plugin(ToolProviderPlugin::new(
            "tools",
            StaticToolProvider::new(vec![ToolBinding::new(
                ToolDescriptor::new("lookup", "lookup", "lookup", json!({"type":"object"})),
                Lookup,
            )]),
        ))
        .build()
        .await
        .unwrap()
}

#[tokio::test]
async fn many_deltas_commit_complete_intermediate_messages_reasoning_and_processed_tools() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.db");
    let store = Arc::new(
        SqliteStateStore::open(&path, Default::default())
            .await
            .unwrap(),
    );
    let agent = build(store.clone(), false).await;
    let mut request = RunRequest::new("session", "original user input");
    request
        .extensions
        .insert(ToolResultProcessorService(Arc::new(Processed)));
    let result = agent.run(request).await.unwrap();
    assert_eq!(result.steps, 2);
    let history = agent
        .framework()
        .history()
        .unwrap()
        .read_session("session", Default::default())
        .await
        .unwrap();
    assert_eq!(history.records.messages.len(), 3);
    assert_eq!(history.records.steps.len(), 2);
    assert_eq!(history.records.tool_invocations.len(), 1);
    for message in &history.records.messages[1..] {
        assert_eq!(message.status, HistoryStatus::Completed);
        assert!(message.content.contains(&HistoryContentBlock::Reasoning {
            text: "r".repeat(4000)
        }));
    }
    assert_eq!(
        history.records.tool_invocations[0].content,
        vec![HistoryContentBlock::Text {
            text: "processed result".into()
        }]
    );
    let public = serde_json::to_string(&history).unwrap();
    assert!(!public.contains("PRIVATE-CONTINUATION"));
    assert!(!public.contains("PRIVATE-CONTENT-WRAPPER"));
    assert!(!public.contains("RAW-RESULT"));
    let db = rusqlite::Connection::open(&path).unwrap();
    let execution_events: u64 = db
        .query_row(
            "SELECT count(*) FROM ef_events WHERE kind='EXECUTION_EVENT'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(execution_events, 0);
    // Ordinary history must remain readable even with unavailable replay and
    // corrupt recovery payloads; a scan/parsing fallback would fail this check.
    db.execute_batch("DROP TABLE ef_events; UPDATE ef_checkpoints SET checkpoint='corrupt';")
        .unwrap();
    let reread = store
        .read_session("session", Default::default())
        .await
        .unwrap();
    assert_eq!(reread.records, history.records);
    agent.framework().close().await;
    let reopened = SqliteStateStore::open(&path, Default::default())
        .await
        .unwrap();
    assert_eq!(
        reopened
            .read_session("session", Default::default())
            .await
            .unwrap()
            .records,
        history.records
    );
}

#[tokio::test]
async fn failed_stream_is_partial_and_never_masquerades_as_complete() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(
        SqliteStateStore::open(directory.path().join("partial.db"), Default::default())
            .await
            .unwrap(),
    );
    let agent = build(store.clone(), true).await;
    assert!(
        agent
            .run(RunRequest::new("partial", "question"))
            .await
            .is_err()
    );
    let page = store
        .read_session("partial", Default::default())
        .await
        .unwrap();
    assert_eq!(page.records.messages.len(), 2);
    assert_eq!(page.records.messages[1].status, HistoryStatus::Interrupted);
    assert_eq!(
        page.records.messages[1]
            .content
            .iter()
            .filter_map(|b| if let HistoryContentBlock::Text { text } = b {
                Some(text.as_str())
            } else {
                None
            })
            .collect::<String>(),
        "t".repeat(4000)
    );
    assert_eq!(page.runs[0].status, HistoryStatus::Failed);
    agent.framework().close().await;
}

#[tokio::test]
async fn checkpoint_and_history_rollback_together_on_failure_and_reject_rewrites() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("atomic.db");
    let store = SqliteStateStore::open(&path, Default::default())
        .await
        .unwrap();
    let mut cp = Checkpoint::new(RunId::new_v4(), "atomic", "fixture", 1, json!({}));
    cp.history
        .messages
        .push(HistoryMessage::user(cp.run_id, &"original".into()));
    let cp = store.create(cp, None).await.unwrap();
    let lease = store
        .claim(cp.run_id, RunId::new_v4(), Duration::from_secs(30))
        .await
        .unwrap();
    let before = store
        .read_session("atomic", Default::default())
        .await
        .unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("CREATE TRIGGER fail_history BEFORE INSERT ON ef_history_messages BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    let mut next = cp.clone();
    next.history
        .messages
        .push(HistoryMessage::user(cp.run_id, &"next".into()));
    assert!(store.commit(&lease, 0, next).await.is_err());
    assert_eq!(store.load(cp.run_id).await.unwrap().revision, 0);
    let after = store
        .read_session("atomic", Default::default())
        .await
        .unwrap();
    assert_eq!(before.version, after.version);
    assert_eq!(before.records, after.records);
    let mut rewrite = cp.clone();
    rewrite.history.messages[0].content = vec![HistoryContentBlock::Text {
        text: "forged".into(),
    }];
    assert!(matches!(
        store.commit(&lease, 0, rewrite).await,
        Err(StateError::Invalid(_))
    ));
}

#[tokio::test]
async fn turn_pages_are_versioned_and_indexed_with_bounded_read_cost() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("pagination.db");
    let store = SqliteStateStore::open(&path, Default::default())
        .await
        .unwrap();
    let mut previous = None;
    for index in 0..1000 {
        let mut cp = Checkpoint::new(RunId::new_v4(), "long", "fixture", 1, json!({}));
        cp.history.messages.push(HistoryMessage::user(
            cp.run_id,
            &format!("turn {index}: {}", "body ".repeat(200)).into(),
        ));
        let cp = store.create(cp, previous).await.unwrap();
        let lease = store
            .claim(cp.run_id, RunId::new_v4(), Duration::from_secs(30))
            .await
            .unwrap();
        let mut done = cp;
        done.status = ExecutionStatus::Completed;
        let done = store.commit(&lease, 0, done).await.unwrap();
        store.release(&lease).await.unwrap();
        previous = Some(done.run_id);
    }
    let query = HistoryQuery {
        limit: 20,
        cursor: None,
    };
    let first = store.read_session("long", query.clone()).await.unwrap();
    assert_eq!(first.records.messages.len(), 20);
    let second = store
        .read_session(
            "long",
            HistoryQuery {
                limit: 20,
                cursor: first.next_cursor.clone(),
            },
        )
        .await
        .unwrap();
    assert_eq!(first.version, second.version);
    assert!(
        first
            .runs
            .iter()
            .all(|a| second.runs.iter().all(|b| a.id != b.id))
    );
    let mut samples = Vec::new();
    for _ in 0..100 {
        let start = Instant::now();
        let page = store.read_session("long", query.clone()).await.unwrap();
        let _ = serde_json::to_vec(&page).unwrap();
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(f64::total_cmp);
    let p95 = samples[94];
    println!(
        "history benchmark: 1000 turns, 20 turns/page, 100 reads+serialization, P95={p95:.3} ms"
    );
    assert!(p95 <= 300.0);
    let mut cp = Checkpoint::new(RunId::new_v4(), "long", "fixture", 1, json!({}));
    cp.history
        .messages
        .push(HistoryMessage::user(cp.run_id, &"new".into()));
    store.create(cp, previous).await.unwrap();
    assert!(matches!(
        store
            .read_session(
                "long",
                HistoryQuery {
                    limit: 20,
                    cursor: first.next_cursor
                }
            )
            .await,
        Err(StateError::Conflict)
    ));
}

#[tokio::test]
async fn history_joins_the_hosts_transaction_and_sees_its_own_writes() {
    let directory = tempfile::tempdir().unwrap();
    let database = SqliteDatabase::open(directory.path().join("joined.db"))
        .await
        .unwrap();
    let store = SqliteStateStore::with_database(database.clone(), Default::default())
        .await
        .unwrap();
    database
        .transaction(async {
            let mut cp = Checkpoint::new(RunId::new_v4(), "joined", "fixture", 1, json!({}));
            cp.history.messages.push(HistoryMessage::user(
                cp.run_id,
                &"uncommitted history".into(),
            ));
            store.create(cp, None).await?;
            let page = store.read_session("joined", Default::default()).await?;
            assert_eq!(page.records.messages.len(), 1);
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        store
            .read_session("joined", Default::default())
            .await
            .unwrap()
            .records
            .messages
            .len(),
        1
    );
}
