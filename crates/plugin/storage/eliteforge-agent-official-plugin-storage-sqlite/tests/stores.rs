use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_storage_sqlite::*;
use std::sync::Arc;

struct Noop;
#[async_trait]
impl AgentLoop for Noop {
    async fn run(
        &self,
        _: &RunRequest,
        _: &RunContext,
        _: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        unreachable!()
    }
}
#[tokio::test]
async fn sqlite_bundle_reopens_conversations_memory_summaries_and_binary_artifacts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("all.db");
    let build = || {
        AgentBuilder::new()
            .plugin(SqliteStoragePlugin::new(&path))
            .plugin(AgentLoopPlugin::new(Noop))
    };
    let agent = build().build().await.unwrap();
    let conversation = ConversationKey::new("chat");
    agent
        .append_conversation(&conversation, 0, vec![Message::user("retained")])
        .await
        .unwrap();
    let snapshot = agent.conversation(&conversation).await.unwrap();
    let entry = agent
        .save_memory(
            "user-a",
            MemoryWrite {
                id: "preference".into(),
                content: "portrait".into(),
                source: MemorySource {
                    id: "chat".into(),
                    version: None,
                },
                expires_at_ms: None,
            },
            None,
        )
        .await
        .unwrap();
    let key = SummaryKey {
        conversation: snapshot.key,
        generation: snapshot.generation,
        block_id: "history".into(),
        policy_version: "1".into(),
        configuration_digest: "a".repeat(64),
    };
    let summary = ConversationSummary {
        format_version: 1,
        source_revision: 1,
        covered_messages: 1,
        source_digest: "b".repeat(64),
        summary: "retained summary".into(),
    };
    assert!(
        agent
            .execute(SaveSummary {
                key: key.clone(),
                expected: None,
                value: summary.clone()
            })
            .await
            .unwrap()
    );
    let bytes = vec![0, 1, 255, 0, 42];
    let artifact = agent.execute(PutArtifact(bytes.clone())).await.unwrap();
    assert!(agent.close().await.is_success());
    assert!(matches!(
        agent.conversation(&conversation).await,
        Err(AgentError::Closed)
    ));
    assert!(matches!(
        agent.execute(PutArtifact(vec![1])).await,
        Err(AgentError::Closed)
    ));
    let agent = build().build().await.unwrap();
    assert_eq!(
        agent.conversation(&conversation).await.unwrap().messages[0]
            .content
            .text(),
        "retained"
    );
    assert_eq!(agent.memory("user-a", "preference").await.unwrap(), entry);
    assert_eq!(
        agent
            .search_memory("user-a", MemoryQuery::new("portrait"))
            .await
            .unwrap(),
        vec![entry.clone()]
    );
    assert_eq!(
        agent
            .execute(LoadSummary(key.clone()))
            .await
            .unwrap()
            .unwrap()
            .value,
        summary
    );
    assert_eq!(
        agent
            .execute(ReadArtifact {
                id: artifact.id.clone(),
                offset: 1,
                length: 3
            })
            .await
            .unwrap()
            .bytes,
        bytes[1..4]
    );
    assert!(
        agent
            .execute(ReadArtifact {
                id: artifact.id.clone(),
                offset: 6,
                length: 1
            })
            .await
            .is_err()
    );
    agent
        .execute(RemoveArtifact(artifact.id.clone()))
        .await
        .unwrap();
    assert!(
        agent
            .execute(ReadArtifact {
                id: artifact.id,
                offset: 0,
                length: 1
            })
            .await
            .is_err()
    );
    let stored = agent
        .execute(LoadSummary(key.clone()))
        .await
        .unwrap()
        .unwrap();
    assert!(
        !agent
            .execute(DeleteSummary {
                key: key.clone(),
                expected: "stale".into()
            })
            .await
            .unwrap()
    );
    assert!(
        agent
            .execute(DeleteSummary {
                key: key.clone(),
                expected: stored.version
            })
            .await
            .unwrap()
    );
    assert!(agent.execute(LoadSummary(key)).await.unwrap().is_none());
    assert!(matches!(
        agent.delete_memory("user-a", "preference", "stale").await,
        Err(AgentError::LongTermMemory(MemoryError::Conflict))
    ));
    agent
        .delete_memory("user-a", "preference", &entry.version)
        .await
        .unwrap();
    assert!(matches!(
        agent.memory("user-a", "preference").await,
        Err(AgentError::LongTermMemory(MemoryError::NotFound))
    ));
    assert!(agent.close().await.is_success());
}

#[tokio::test]
async fn sqlite_interaction_answers_have_atomic_competition_and_idempotency() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("answers.db");
    let a = Arc::new(
        eliteforge_agent_state_sqlite::SqliteStateStore::open(&path, Default::default())
            .await
            .unwrap(),
    );
    let b = Arc::new(
        eliteforge_agent_state_sqlite::SqliteStateStore::open(&path, Default::default())
            .await
            .unwrap(),
    );
    let request =
        InteractionRequest::input("Choose", serde_json::json!({"type":"string"})).unwrap();
    let record = InteractionRecord {
        session_id: "s".into(),
        run_id: RunId::new_v4(),
        node_invocation_id: None,
        request: request.clone(),
        tool_call: None,
        response: None,
        created_at_unix_ms: unix_time_ms(),
        answered_at_unix_ms: None,
    };
    a.publish(&record).await.unwrap();
    let one = InteractionResponse {
        interaction_id: request.id,
        value: "one".into(),
    };
    let two = InteractionResponse {
        interaction_id: request.id,
        value: "two".into(),
    };
    let (first, second) = tokio::join!(a.answer("s", &one), b.answer("s", &two));
    assert_ne!(first.is_ok(), second.is_ok());
    let stored = a.get("s", request.id).await.unwrap();
    assert!(
        a.answer("s", stored.response.as_ref().unwrap())
            .await
            .is_ok()
    );
    assert!(b.get("other", request.id).await.is_err());
    let mut conflicting = record.clone();
    conflicting.run_id = RunId::new_v4();
    assert!(a.publish(&conflicting).await.is_err());
    let mut expired = record;
    expired.request.id = RunId::new_v4();
    expired.request.expires_at_unix_ms = Some(1);
    a.publish(&expired).await.unwrap();
    assert!(
        a.answer(
            "s",
            &InteractionResponse {
                interaction_id: expired.request.id,
                value: "late".into()
            }
        )
        .await
        .is_err()
    );
    a.close().await.unwrap();
    b.close().await.unwrap();
}

#[cfg(all(
    feature = "bundled-native",
    any(
        target_os = "macos",
        all(target_os = "linux", target_env = "gnu"),
        all(target_os = "windows", target_arch = "x86_64")
    )
))]
#[tokio::test]
async fn sqlite_plugin_prepares_extensions_on_every_physical_connection() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = ExtensionConfig::new(dir.path().join("native"));
    config.simple = ExtensionMode::Required;
    config.vector = ExtensionMode::Required;
    let agent = AgentBuilder::new()
        .plugin(SqliteStoragePlugin::new(dir.path().join("agent.db")).with_extensions(config))
        .plugin(AgentLoopPlugin::new(Noop))
        .build()
        .await
        .unwrap();
    let capabilities = agent.execute(ReadSqliteCapabilities).await.unwrap();
    assert_eq!(capabilities.len(), 5);
    for (_, report) in &capabilities {
        assert!(report.fts5);
        assert_eq!(report.simple, ExtensionStatus::Available);
        assert_eq!(report.vector, ExtensionStatus::Available);
    }
    assert!(agent.close().await.is_success());
}
