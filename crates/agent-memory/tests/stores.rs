use agent_framework::*;
use agent_memory::*;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn value(id: &str, content: &str) -> MemoryWrite {
    MemoryWrite {
        id: id.into(),
        content: content.into(),
        source: MemorySource {
            id: "conversation:chat-1/message-3".into(),
            version: Some("3".into()),
        },
        expires_at_ms: None,
    }
}
async fn verify_memory(memory: &dyn LongTermMemory) {
    let first = memory
        .save("a", &value("preference", "Rust Tokyo video%_\\"), None)
        .await
        .unwrap();
    assert!(matches!(
        memory.get("b", "preference").await,
        Err(MemoryError::NotFound)
    ));
    assert!(matches!(
        memory.save("a", &first.value, None).await,
        Err(MemoryError::Conflict)
    ));
    assert_eq!(
        memory
            .search("a", &MemoryQuery::new("RUST tokyo"))
            .await
            .unwrap(),
        std::slice::from_ref(&first)
    );
    assert_eq!(
        memory.search("a", &MemoryQuery::new("%_\\")).await.unwrap(),
        std::slice::from_ref(&first),
        "SQL wildcards are literal search terms"
    );
    let updated = memory
        .save(
            "a",
            &value("preference", "Rust Kyoto"),
            Some(&first.version),
        )
        .await
        .unwrap();
    assert_ne!(updated.version, first.version);
    assert!(
        memory
            .search("a", &MemoryQuery::new("Tokyo"))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        memory.delete("a", "preference", &first.version).await,
        Err(MemoryError::Conflict)
    ));
    memory
        .delete("a", "preference", &updated.version)
        .await
        .unwrap();
    assert!(
        memory
            .search("a", &MemoryQuery::new("Rust"))
            .await
            .unwrap()
            .is_empty()
    );
    let recreated = memory
        .save("a", &value("preference", "Rust latest"), None)
        .await
        .unwrap();
    assert_ne!(recreated.version, updated.version);
    assert!(matches!(
        memory
            .save("a", &value("preference", "stale"), Some(&updated.version))
            .await,
        Err(MemoryError::Conflict)
    ));
    let mut small = MemoryQuery::new("Rust");
    small.max_bytes = 1;
    assert!(memory.search("a", &small).await.unwrap().is_empty());
    let mut expiring = value("temporary", "temporary note");
    expiring.expires_at_ms = Some(
        (SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
            + 100) as u64,
    );
    let old = memory.save("a", &expiring, None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(matches!(
        memory.get("a", "temporary").await,
        Err(MemoryError::NotFound)
    ));
    assert!(
        memory
            .search("a", &MemoryQuery::new("temporary"))
            .await
            .unwrap()
            .is_empty()
    );
    let new = memory
        .save("a", &value("temporary", "new temporary"), None)
        .await
        .unwrap();
    assert_ne!(new.version, old.version);
}
#[tokio::test]
async fn default_backends_isolate_namespaces_versions_expiry_and_search_bounds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let memory = SqliteLongTermMemory::open(&path, MemoryStoreOptions::default())
        .await
        .unwrap();
    verify_memory(&memory).await;
    let retained = memory.get("a", "preference").await.unwrap();
    memory.close().await.unwrap();
    let reopened = SqliteLongTermMemory::open(&path, MemoryStoreOptions::default())
        .await
        .unwrap();
    assert_eq!(reopened.get("a", "preference").await.unwrap(), retained);
    reopened.close().await.unwrap();
}
#[tokio::test]
async fn sqlite_memory_concurrent_updates_have_one_winner_and_bound_storage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let options = MemoryStoreOptions {
        max_entries: 1,
        max_entry_bytes: 1024,
        ..Default::default()
    };
    let a = SqliteLongTermMemory::open(&path, options.clone())
        .await
        .unwrap();
    let b = SqliteLongTermMemory::open(&path, options).await.unwrap();
    let initial = a.save("a", &value("one", "initial"), None).await.unwrap();
    let first = value("one", "first");
    let second = value("one", "second");
    let (x, y) = tokio::join!(
        a.save("a", &first, Some(&initial.version)),
        b.save("a", &second, Some(&initial.version))
    );
    assert_ne!(x.is_ok(), y.is_ok());
    assert!(matches!(
        a.save("a", &value("two", "new"), None).await,
        Err(MemoryError::Capacity)
    ));
    assert!(
        a.save("a", &value("one", &"x".repeat(2000)), None)
            .await
            .is_err()
    );
    assert!(
        a.search(
            "a",
            &MemoryQuery {
                limit: usize::MAX,
                ..MemoryQuery::new("")
            }
        )
        .await
        .is_err()
    );
    a.close().await.unwrap();
    b.close().await.unwrap();
}

fn exchange() -> Vec<Message> {
    let mut call = Message::assistant("thinking");
    call.tool_calls.push(ToolCall::new(
        "call-1",
        "lookup",
        serde_json::json!({"key":42}),
    ));
    call.model_context = Some(ModelMessageContext {
        model: ModelRef::new("provider", "model"),
        format: "signed".into(),
        data: serde_json::json!({"signature":"exact"}),
    });
    let mut result = Message::tool(
        "call-1",
        Content(vec![ContentBlock::Resource {
            resource: ResourceReference {
                uri: "artifact:result-1".into(),
                mime_type: Some("application/json".into()),
                name: None,
                size_bytes: Some(123),
            },
        }]),
    );
    result.tool_result = Some(ToolResultMetadata {
        is_error: true,
        structured: None,
        result_reference: Some(Box::new(ResourceReference {
            uri: "artifact:result-1".into(),
            mime_type: Some("application/json".into()),
            name: None,
            size_bytes: Some(123),
        })),
    });
    let mut final_message = Message::assistant("done");
    final_message.structured = Some(serde_json::json!({"ok":false}));
    vec![Message::user("input"), call, result, final_message]
}
#[tokio::test]
async fn sqlite_conversation_roundtrips_identity_and_atomic_structured_batches() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("conversations.db");
    let options = SqliteConversationOptions {
        max_messages: 8,
        max_bytes: 4096,
        ..Default::default()
    };
    let store = SqliteConversationStore::open(&path, options.clone())
        .await
        .unwrap();
    let mut session = ConversationStore::open(&store, "s").await.unwrap();
    let messages = exchange();
    session.append_at(0, &messages).await.unwrap();
    let snapshot = session.snapshot().await.unwrap();
    assert_eq!(snapshot.messages, messages);
    assert!(
        session
            .append_at(0, &[Message::assistant("stale")])
            .await
            .is_err()
    );
    assert!(
        session
            .append(&[Message::tool("missing", "orphan")])
            .await
            .is_err()
    );
    assert!(
        session
            .append(&[Message::user("x".repeat(5000))])
            .await
            .is_err()
    );
    assert_eq!(session.snapshot().await.unwrap().messages, messages);
    session
        .finish(ConversationOutcome::Completed)
        .await
        .unwrap();
    drop(session);
    store.close().await.unwrap();
    let store = SqliteConversationStore::open(&path, options).await.unwrap();
    let mut session = ConversationStore::open(&store, "s").await.unwrap();
    let reopened = session.snapshot().await.unwrap();
    assert_eq!(reopened.generation, snapshot.generation);
    assert_eq!(reopened.message_id(2), snapshot.message_id(2));
    assert_eq!(reopened.messages, messages);
    let mut branch = store
        .open_branch(&ConversationKey::new("s").branch("node-b"))
        .await
        .unwrap();
    assert_eq!(branch.snapshot().await.unwrap().revision, 0);
    branch.finish(ConversationOutcome::Completed).await.unwrap();
    session
        .finish(ConversationOutcome::Completed)
        .await
        .unwrap();
    store.close().await.unwrap();
}
#[tokio::test]
async fn conversation_renewal_keeps_fencing_identity_and_takeover_rejects_old_session() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("conversations.db");
    let options = SqliteConversationOptions {
        lease_duration: Duration::from_millis(300),
        ..Default::default()
    };
    let a = SqliteConversationStore::open(&path, options.clone())
        .await
        .unwrap();
    let b = SqliteConversationStore::open(&path, options).await.unwrap();
    let mut first = ConversationStore::open(&a, "s").await.unwrap();
    first.append(&[Message::user("first")]).await.unwrap();
    let inspector = tokio_rusqlite::Connection::open(&path).await.unwrap();
    let owner: String = inspector
        .call(|connection| {
            connection.query_row(
                "SELECT owner FROM conversations WHERE session='s'",
                [],
                |r| r.get(0),
            )
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(650)).await;
    let renewed: String = inspector
        .call(|connection| {
            connection.query_row(
                "SELECT owner FROM conversations WHERE session='s'",
                [],
                |r| r.get(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(owner, renewed);
    assert!(matches!(
        ConversationStore::open(&b, "s").await,
        Err(ConversationError::Conflict)
    ));
    inspector
        .call(|connection| {
            connection.execute(
                "UPDATE conversations SET lease_until=0 WHERE session='s'",
                [],
            )
        })
        .await
        .unwrap();
    let mut second = ConversationStore::open(&b, "s").await.unwrap();
    assert!(matches!(
        first.append(&[Message::assistant("stale")]).await,
        Err(ConversationError::Conflict)
    ));
    // Releasing the old lease cannot release the new owner.
    first.finish(ConversationOutcome::Failed).await.unwrap();
    second
        .append(&[Message::assistant("new owner")])
        .await
        .unwrap();
    assert_eq!(second.snapshot().await.unwrap().messages.len(), 2);
    second.finish(ConversationOutcome::Completed).await.unwrap();
    inspector.close().await.unwrap();
    a.close().await.unwrap();
    b.close().await.unwrap();
}
#[tokio::test]
async fn inactive_conversation_expiry_changes_generation_and_active_sessions_survive() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteConversationStore::open(
        dir.path().join("history.db"),
        SqliteConversationOptions {
            session_ttl: Duration::from_millis(20),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let mut first = ConversationStore::open(&store, "s").await.unwrap();
    first.append(&[Message::user("old")]).await.unwrap();
    let generation = first.snapshot().await.unwrap().generation;
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(
        ConversationStore::open(&store, "s").await.is_err(),
        "active lease survives retention TTL"
    );
    first.finish(ConversationOutcome::Completed).await.unwrap();
    tokio::time::sleep(Duration::from_millis(40)).await;
    let mut second = ConversationStore::open(&store, "s").await.unwrap();
    let snapshot = second.snapshot().await.unwrap();
    assert_ne!(snapshot.generation, generation);
    assert!(snapshot.messages.is_empty());
    second.finish(ConversationOutcome::Completed).await.unwrap();
    store.close().await.unwrap();
}
