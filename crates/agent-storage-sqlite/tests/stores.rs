use agent_context::*;
use agent_framework::*;
use agent_storage_sqlite::*;
use async_trait::async_trait;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

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
    let conversations = agent
        .service::<ConversationStoreService>()
        .unwrap()
        .0
        .clone();
    let mut session = conversations.open("chat").await.unwrap();
    session.append(&[Message::user("retained")]).await.unwrap();
    let snapshot = session.snapshot().await.unwrap();
    session
        .finish(ConversationOutcome::Completed)
        .await
        .unwrap();
    let memory = agent.service::<LongTermMemoryService>().unwrap().0.clone();
    let entry = memory
        .save(
            "user-a",
            &MemoryWrite {
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
    let summaries = agent.service::<SummaryStoreService>().unwrap().0.clone();
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
        summaries
            .compare_exchange(&key, None, &summary)
            .await
            .unwrap()
    );
    let artifacts = agent.service::<ArtifactStoreService>().unwrap().0.clone();
    let bytes = vec![0, 1, 255, 0, 42];
    let artifact = artifacts
        .put(bytes.clone(), CancellationToken::new())
        .await
        .unwrap();
    assert!(agent.close().await.is_success());
    assert!(conversations.open("chat").await.is_err());
    let agent = build().build().await.unwrap();
    let mut session = agent
        .service::<ConversationStoreService>()
        .unwrap()
        .0
        .open("chat")
        .await
        .unwrap();
    assert_eq!(
        session.snapshot().await.unwrap().messages[0].content.text(),
        "retained"
    );
    session
        .finish(ConversationOutcome::Completed)
        .await
        .unwrap();
    assert_eq!(
        agent
            .service::<LongTermMemoryService>()
            .unwrap()
            .0
            .get("user-a", "preference")
            .await
            .unwrap(),
        entry
    );
    assert_eq!(
        agent
            .service::<SummaryStoreService>()
            .unwrap()
            .0
            .load(&key)
            .await
            .unwrap()
            .unwrap()
            .value,
        summary
    );
    let artifacts = agent.service::<ArtifactStoreService>().unwrap().0.clone();
    assert_eq!(
        artifacts
            .read(&artifact.id, 1, 3, CancellationToken::new())
            .await
            .unwrap()
            .bytes,
        bytes[1..4]
    );
    assert!(matches!(
        artifacts
            .read(&artifact.id, 6, 1, CancellationToken::new())
            .await,
        Err(ArtifactError::InvalidRange)
    ));
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(
        artifacts.put(vec![1], cancelled).await,
        Err(ArtifactError::Cancelled)
    ));
    artifacts
        .remove(&artifact.id, CancellationToken::new())
        .await
        .unwrap();
    assert!(matches!(
        artifacts
            .read(&artifact.id, 0, 1, CancellationToken::new())
            .await,
        Err(ArtifactError::NotFound)
    ));
    assert!(agent.close().await.is_success());
}

#[tokio::test]
async fn sqlite_interaction_answers_have_atomic_competition_and_idempotency() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("answers.db");
    let a = Arc::new(
        agent_state_sqlite::SqliteStateStore::open(&path, Default::default())
            .await
            .unwrap(),
    );
    let b = Arc::new(
        agent_state_sqlite::SqliteStateStore::open(&path, Default::default())
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
    let capabilities = agent.service::<SqliteCapabilitiesService>().unwrap();
    assert_eq!(capabilities.0.len(), 5);
    for (_, report) in &capabilities.0 {
        assert!(report.fts5);
        assert_eq!(report.simple, ExtensionStatus::Available);
        assert_eq!(report.vector, ExtensionStatus::Available);
    }
    assert!(agent.close().await.is_success());
}
