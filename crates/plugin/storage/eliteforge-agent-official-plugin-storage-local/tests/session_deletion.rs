use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_storage_local::*;
use eliteforge_agent_test_support::TestBuilder;
use rusqlite::{Connection, params};
use serde_json::json;
use std::{path::PathBuf, sync::Arc, time::Duration};

struct Noop;
#[async_trait]
impl AgentLoop for Noop {
    fn requires_conversations(&self) -> bool {
        false
    }

    async fn run(
        &self,
        _: &RunRequest,
        _: &RunContext,
        _: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        unreachable!()
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
    artifacts: PathBuf,
    agent: Agent,
    state: Arc<SqliteStateStore>,
}

impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        let artifacts = dir.path().join("artifacts");
        std::fs::create_dir(&artifacts).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&artifacts, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let agent = TestBuilder::new()
            .plugin(LocalStoragePlugin::new(&path, &artifacts))
            .plugin(AgentLoopPlugin::new(Noop))
            .build()
            .await
            .unwrap();
        let state = Arc::new(
            SqliteStateStore::open(&path, Default::default())
                .await
                .unwrap(),
        );
        Self {
            _dir: dir,
            path,
            artifacts,
            agent,
            state,
        }
    }

    async fn seed(&self, id: &str) -> (Vec<RunId>, Vec<SummaryKey>, RunId) {
        let mut first = Checkpoint::new(RunId::new_v4(), id, "test", 1, json!({}));
        first.transcript.push(Message::user("retained history"));
        let mut first = self.state.create(first, None).await.unwrap();
        let lease = self
            .state
            .claim(first.run_id, RunId::new_v4(), Duration::from_secs(30))
            .await
            .unwrap();
        first.status = ExecutionStatus::Completed;
        let first = self.state.commit(&lease, 0, first).await.unwrap();
        self.state.release(&lease).await.unwrap();
        let mut second = Checkpoint::new(RunId::new_v4(), id, "test", 1, json!({}));
        second.transcript = first.transcript.clone();
        let second = self.state.create(second, Some(first.run_id)).await.unwrap();
        let mut keys = Vec::new();
        for branch in ["main", "alternative"] {
            let key = ConversationKey::new(id).branch(branch);
            self.agent
                .append_conversation(&key, 0, vec![Message::user("history")])
                .await
                .unwrap();
            let snapshot = self.agent.conversation(&key).await.unwrap();
            // Also keep a summary from an older generation of the same branch.
            for generation in [snapshot.generation, "old-generation".into()] {
                let key = SummaryKey {
                    conversation: key.clone(),
                    generation,
                    block_id: "history".into(),
                    policy_version: "1".into(),
                    configuration_digest: "a".repeat(64),
                };
                assert!(
                    self.agent
                        .execute(SaveSummary {
                            key: key.clone(),
                            expected: None,
                            value: ConversationSummary {
                                format_version: 1,
                                source_revision: 1,
                                covered_messages: 1,
                                source_digest: "b".repeat(64),
                                summary: "summary".into(),
                            },
                        })
                        .await
                        .unwrap()
                );
                keys.push(key);
            }
        }
        let request = InteractionRequest::input("Choose", json!({"type":"string"})).unwrap();
        self.state
            .publish(&InteractionRecord {
                session_id: id.into(),
                run_id: second.run_id,
                request: request.clone(),
                tool_call: None,
                response: None,
                response_provenance: None,
                created_at_unix_ms: unix_time_ms(),
                answered_at_unix_ms: None,
            })
            .await
            .unwrap();
        // Both answered and pending interactions belong to the session.
        self.state
            .answer(
                id,
                &InteractionResponse {
                    interaction_id: request.id,
                    value: json!("answer"),
                },
            )
            .await
            .unwrap();
        let mut pending = self.state.get(id, request.id).await.unwrap();
        pending.request.id = RunId::new_v4();
        pending.response = None;
        pending.answered_at_unix_ms = None;
        self.state.publish(&pending).await.unwrap();
        (vec![first.run_id, second.run_id], keys, request.id)
    }

    fn counts(&self, id: &str) -> Vec<usize> {
        let connection = Connection::open(&self.path).unwrap();
        [
            "SELECT count(*) FROM ef_session_heads WHERE session_id=?1",
            "SELECT count(*) FROM ef_checkpoints WHERE session_id=?1",
            "SELECT count(*) FROM ef_interactions WHERE session_id=?1",
            "SELECT count(*) FROM ef_conversations WHERE session=?1",
            "SELECT count(*) FROM ef_context_summaries WHERE json_extract(key, '$.conversation.session_id')=?1",
        ].iter().map(|sql| connection.query_row(sql, [id], |row| row.get(0)).unwrap()).collect()
    }

    async fn close(&self) {
        assert!(self.agent.framework().close().await.is_success());
        self.state.close().await.unwrap();
    }
}

#[tokio::test]
async fn single_delete_removes_all_session_data_and_survives_reopen() {
    let fixture = Fixture::new().await;
    let id = "chat'\"%_你好";
    let (runs, keys, interaction) = fixture.seed(id).await;
    fixture.seed(&format!("{id}-other")).await;
    let entry = fixture
        .agent
        .save_memory(
            "shared",
            MemoryWrite {
                id: "preference".into(),
                content: "retain".into(),
                source: MemorySource {
                    id: id.into(),
                    version: None,
                },
                expires_at_ms: None,
            },
            None,
        )
        .await
        .unwrap();
    let artifact = fixture
        .agent
        .execute(PutArtifact(vec![1, 2, 3]))
        .await
        .unwrap();
    assert_eq!(fixture.counts(id), vec![1, 2, 2, 2, 4]);
    assert!(
        fixture
            .agent
            .execute(DeleteSession(id.into()))
            .await
            .unwrap()
    );
    assert_eq!(fixture.counts(id), vec![0; 5]);
    assert_eq!(fixture.counts(&format!("{id}-other")), vec![1, 2, 2, 2, 4]);
    assert!(fixture.state.latest(id).await.unwrap().is_none());
    for run in runs {
        assert!(matches!(
            fixture.state.load(run).await,
            Err(StateError::NotFound)
        ));
    }
    assert!(matches!(
        fixture.state.get(id, interaction).await,
        Err(StateError::NotFound)
    ));
    assert!(fixture.state.pending(id).await.unwrap().is_empty());
    for key in keys {
        assert!(
            fixture
                .agent
                .execute(LoadSummary(key))
                .await
                .unwrap()
                .is_none()
        );
    }
    assert!(
        !fixture
            .agent
            .execute(DeleteSession(id.into()))
            .await
            .unwrap()
    );
    fixture.close().await;
    let reopened = TestBuilder::new()
        .plugin(LocalStoragePlugin::new(&fixture.path, &fixture.artifacts))
        .plugin(AgentLoopPlugin::new(Noop))
        .build()
        .await
        .unwrap();
    assert!(!reopened.execute(DeleteSession(id.into())).await.unwrap());
    assert_eq!(
        reopened.memory("shared", "preference").await.unwrap(),
        entry
    );
    assert_eq!(
        reopened
            .execute(ReadArtifact {
                id: artifact.id,
                offset: 0,
                length: 3
            })
            .await
            .unwrap()
            .bytes,
        vec![1, 2, 3]
    );
    assert!(
        reopened
            .conversation(&ConversationKey::new(id))
            .await
            .is_err()
    );
    assert!(reopened.framework().close().await.is_success());
}

#[tokio::test]
async fn batch_is_exact_idempotent_and_not_limited_by_sql_parameter_count() {
    let fixture = Fixture::new().await;
    for id in ["a", " a ", "b", "keep"] {
        fixture.seed(id).await;
    }
    let empty = fixture.agent.execute(DeleteSessions(vec![])).await.unwrap();
    assert_eq!(empty.deleted, 0);
    assert!(empty.failures.is_empty());
    let mut ids: Vec<String> = (0..1100).map(|index| format!("missing-{index}")).collect();
    ids.extend(["a".into(), " a ".into(), "a".into(), "b".into()]);
    let report = fixture
        .agent
        .execute(DeleteSessions(ids.clone()))
        .await
        .unwrap();
    assert_eq!(report.deleted, 3);
    assert!(report.failures.is_empty());
    let retry = fixture.agent.execute(DeleteSessions(ids)).await.unwrap();
    assert_eq!(retry.deleted, 0);
    assert!(retry.failures.is_empty());
    for id in ["a", " a ", "b"] {
        assert_eq!(fixture.counts(id), vec![0; 5]);
    }
    assert_eq!(fixture.counts("keep"), vec![1, 2, 2, 2, 4]);
    fixture.close().await;
}

#[tokio::test]
async fn invalid_ids_are_reported_and_do_not_stop_later_deletions() {
    let fixture = Fixture::new().await;
    for invalid in ["", " \t\n"] {
        let id = format!("a-{}", invalid.len());
        fixture.seed(&id).await;
        let report = fixture
            .agent
            .execute(DeleteSessions(vec![
                invalid.into(),
                id.clone(),
                invalid.into(),
            ]))
            .await
            .unwrap();
        assert_eq!(report.deleted, 1);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].session_id, invalid);
        assert!(matches!(report.failures[0].error, StateError::Invalid(_)));
        assert!(matches!(
            fixture.agent.execute(DeleteSession(invalid.into())).await,
            Err(AgentError::State(StateError::Invalid(_)))
        ));
        assert_eq!(fixture.counts(&id), vec![0; 5]);
    }
    fixture.close().await;
}

#[tokio::test]
async fn execution_lease_failure_keeps_successes_and_continues_across_connections() {
    let fixture = Fixture::new().await;
    fixture.seed("a").await;
    let (runs, _, _) = fixture.seed("b").await;
    fixture.seed("c").await;
    let run = runs[1];
    let lease = fixture
        .state
        .claim(run, RunId::new_v4(), Duration::from_secs(30))
        .await
        .unwrap();
    let before = fixture.state.load(run).await.unwrap();
    let report = fixture
        .agent
        .execute(DeleteSessions(vec!["a".into(), "b".into(), "c".into()]))
        .await
        .unwrap();
    assert_eq!(report.deleted, 2);
    assert_eq!(report.failures.len(), 1);
    assert_eq!(report.failures[0].session_id, "b");
    assert!(matches!(report.failures[0].error, StateError::Busy));
    for id in ["a", "c"] {
        assert_eq!(fixture.counts(id), vec![0; 5]);
    }
    assert_eq!(fixture.counts("b"), vec![1, 2, 2, 2, 4]);
    fixture.state.release(&lease).await.unwrap();
    let retry = fixture
        .agent
        .execute(DeleteSessions(vec!["a".into(), "b".into()]))
        .await
        .unwrap();
    assert_eq!(retry.deleted, 1);
    assert!(retry.failures.is_empty());
    assert!(matches!(
        fixture.state.commit(&lease, before.revision, before).await,
        Err(StateError::NotFound)
    ));
    fixture.close().await;
}

#[tokio::test]
async fn live_conversation_branch_is_reported_without_blocking_other_sessions() {
    let fixture = Fixture::new().await;
    fixture.seed("a").await;
    fixture.seed("b").await;
    let conversations = SqliteConversationStore::open(&fixture.path, Default::default())
        .await
        .unwrap();
    let mut session = conversations
        .open_branch(&ConversationKey::new("b").branch("alternative"))
        .await
        .unwrap();
    let report = fixture
        .agent
        .execute(DeleteSessions(vec!["b".into(), "a".into()]))
        .await
        .unwrap();
    assert_eq!(report.deleted, 1);
    assert_eq!(report.failures.len(), 1);
    assert_eq!(report.failures[0].session_id, "b");
    assert!(matches!(report.failures[0].error, StateError::Busy));
    assert_eq!(fixture.counts("a"), vec![0; 5]);
    assert_eq!(fixture.counts("b"), vec![1, 2, 2, 2, 4]);
    session
        .finish(ConversationOutcome::Completed)
        .await
        .unwrap();
    let retry = fixture
        .agent
        .execute(DeleteSessions(vec!["a".into(), "b".into()]))
        .await
        .unwrap();
    assert_eq!(retry.deleted, 1);
    assert!(retry.failures.is_empty());
    assert!(matches!(
        session.append(&[Message::user("stale")]).await,
        Err(ConversationError::Conflict)
    ));
    conversations.close().await.unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn sql_failure_rolls_back_only_its_session_and_preserves_other_successes() {
    let fixture = Fixture::new().await;
    fixture.seed("a").await;
    fixture.seed("b").await;
    fixture.seed("c").await;
    let connection = Connection::open(&fixture.path).unwrap();
    connection.execute_batch("CREATE TRIGGER reject_delete BEFORE DELETE ON ef_interactions WHEN OLD.session_id='b' BEGIN SELECT RAISE(ABORT, 'test failure'); END;").unwrap();
    let report = fixture
        .agent
        .execute(DeleteSessions(vec!["a".into(), "b".into(), "c".into()]))
        .await
        .unwrap();
    assert_eq!(report.deleted, 2);
    assert_eq!(report.failures.len(), 1);
    assert_eq!(report.failures[0].session_id, "b");
    assert!(matches!(report.failures[0].error, StateError::Backend(_)));
    for id in ["a", "c"] {
        assert_eq!(fixture.counts(id), vec![0; 5]);
    }
    assert_eq!(fixture.counts("b"), vec![1, 2, 2, 2, 4]);
    assert!(fixture.state.latest("b").await.unwrap().is_some());
    connection
        .execute_batch("DROP TRIGGER reject_delete")
        .unwrap();
    let retry = fixture
        .agent
        .execute(DeleteSessions(vec!["a".into(), "b".into()]))
        .await
        .unwrap();
    assert_eq!(retry.deleted, 1);
    assert!(retry.failures.is_empty());
    fixture.close().await;
}

#[tokio::test]
async fn batch_returns_every_failure_in_input_order_and_deduplicates_retries() {
    let fixture = Fixture::new().await;
    let (runs, _, _) = fixture.seed("busy").await;
    fixture.seed("broken").await;
    fixture.seed("ok").await;
    let lease = fixture
        .state
        .claim(runs[1], RunId::new_v4(), Duration::from_secs(30))
        .await
        .unwrap();
    Connection::open(&fixture.path).unwrap().execute_batch(
        "CREATE TRIGGER reject_delete BEFORE DELETE ON ef_interactions WHEN OLD.session_id='broken' BEGIN SELECT RAISE(ABORT, 'test failure'); END;"
    ).unwrap();
    let report = fixture
        .agent
        .execute(DeleteSessions(vec![
            "busy".into(),
            "".into(),
            "broken".into(),
            "ok".into(),
            "busy".into(),
            "".into(),
            "missing".into(),
        ]))
        .await
        .unwrap();
    assert_eq!(report.deleted, 1);
    assert_eq!(
        report
            .failures
            .iter()
            .map(|failure| failure.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["busy", "", "broken"]
    );
    assert!(matches!(report.failures[0].error, StateError::Busy));
    assert!(matches!(report.failures[1].error, StateError::Invalid(_)));
    assert!(matches!(report.failures[2].error, StateError::Backend(_)));
    assert_eq!(fixture.counts("ok"), vec![0; 5]);
    for id in ["busy", "broken"] {
        assert_eq!(fixture.counts(id), vec![1, 2, 2, 2, 4]);
    }
    fixture.state.release(&lease).await.unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn execution_only_plugin_supports_cleanup_and_expired_leases_are_fenced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("execution.db");
    let state = Arc::new(
        SqliteStateStore::open(&path, Default::default())
            .await
            .unwrap(),
    );
    let agent = TestBuilder::new()
        .plugin(state.clone().plugin())
        .plugin(AgentLoopPlugin::new(Noop))
        .build()
        .await
        .unwrap();
    let snapshot = state
        .create(
            Checkpoint::new(RunId::new_v4(), "expired", "test", 1, json!({})),
            None,
        )
        .await
        .unwrap();
    let lease = state
        .claim(snapshot.run_id, RunId::new_v4(), Duration::from_secs(30))
        .await
        .unwrap();
    // Expire the authoritative SQL clock snapshot without a timing-dependent sleep.
    Connection::open(path)
        .unwrap()
        .execute(
            "UPDATE ef_checkpoints SET expires=0 WHERE run_id=?1",
            params![snapshot.run_id.to_string()],
        )
        .unwrap();
    assert!(state.delete_session("expired").await.unwrap());
    assert!(matches!(
        state.commit(&lease, 0, snapshot).await,
        Err(StateError::NotFound)
    ));
    let fresh = state
        .create(
            Checkpoint::new(RunId::new_v4(), "fresh", "test", 1, json!({})),
            None,
        )
        .await
        .unwrap();
    assert!(agent.execute(DeleteSession("fresh".into())).await.unwrap());
    assert!(matches!(
        state.load(fresh.run_id).await,
        Err(StateError::NotFound)
    ));
    assert!(agent.framework().close().await.is_success());
    state.close().await.unwrap();
}
