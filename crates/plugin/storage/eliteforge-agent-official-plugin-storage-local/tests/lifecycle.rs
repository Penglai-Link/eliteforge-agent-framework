use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_storage_local::*;
use serde_json::json;
use std::{sync::Arc, time::Duration};
use tokio::sync::Notify;

struct Fixture {
    _dir: tempfile::TempDir,
    database: SqliteDatabase,
    framework: Framework,
}
impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("artifacts");
        std::fs::create_dir(&root).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let path = dir.path().join("shared.db");
        let database = SqliteDatabase::open(&path).await.unwrap();
        database
            .call(|c| c.execute_batch("CREATE TABLE tron_links(id TEXT PRIMARY KEY)"))
            .await
            .unwrap();
        let framework = Framework::builder()
            .default_models(false)
            .plugin(LocalStoragePlugin::new(&path, &root).with_database(database.clone()))
            .plugin(AgentLoopPlugin::named("worker", Echo))
            .plugin(AgentLoopPlugin::named("react", ReactAgentLoop))
            .plugin(AgentLoopPlugin::named(
                "workflow",
                WorkflowLoop::new(WorkflowDefinition {
                    id: "test.workflow".into(),
                    version: 1,
                    stages: vec![WorkflowStage::new("work")],
                })
                .unwrap(),
            ))
            .plugin(AgentLoopPlugin::named(
                "dag",
                DagWorkflowLoop::new(DagWorkflowDefinition {
                    id: "test.dag".into(),
                    version: 1,
                    nodes: vec![DagNode::react(WorkflowStage::new("work"))],
                    edges: vec![],
                    max_parallel_nodes: 1,
                })
                .unwrap(),
            ))
            .plugin(AgentLoopPlugin::named("external", ExternalAgentLoop))
            .plugin(ExternalAgentPlugin::new(FakeExternal))
            .plugin(ChatModelPlugin::new(Provider))
            .plugin(GoalPlugin)
            .defaults(AgentSelection {
                agent_loop: Some("worker".into()),
                external_agent: Some("framework.external-agent".into()),
                ..Default::default()
            })
            .build()
            .await
            .unwrap();
        Self {
            _dir: dir,
            database,
            framework,
        }
    }
    async fn worker(&self) -> Agent {
        self.framework
            .agent(Default::default())
            .build()
            .await
            .unwrap()
    }
    async fn count(&self, table: &'static str) -> i64 {
        self.database
            .call(move |c| c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0)))
            .await
            .unwrap()
    }
    async fn session(&self) -> SessionRecord {
        self.framework
            .sessions()
            .unwrap()
            .create(SessionRecord::new("test"))
            .await
            .unwrap()
    }
}

struct Echo;
#[async_trait]
impl AgentLoop for Echo {
    fn requires_conversations(&self) -> bool {
        false
    }
    async fn run(
        &self,
        request: &RunRequest,
        context: &RunContext,
        _: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        context.emit(AgentEvent::TextDelta("executed".into())).await;
        Ok(RunResult {
            output: request.input.text(),
            content: request.input.clone(),
            structured: None,
            interactions: vec![],
            status: RunStatus::Completed,
            usage: Usage::default(),
            steps: 1,
            tool_calls: 0,
            external_session: None,
        })
    }
}
struct Judge;
#[async_trait]
impl GoalPolicy for Judge {
    async fn evaluate(
        &self,
        evidence: &GoalEvidence,
        _: &RunContext,
    ) -> Result<GoalDecision, AgentError> {
        Ok(GoalDecision {
            verdict: if evidence.goal.runs_used == 0 {
                GoalVerdict::Continue
            } else {
                GoalVerdict::Completed
            },
            reason: "observed execution".into(),
            next_input: Some("second".into()),
            progress: json!({"runs":evidence.goal.runs_used+1}),
            required_input: None,
        })
    }
}

struct Provider;
#[async_trait]
impl ChatModel for Provider {
    async fn generate(
        &self,
        _: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        let content = if request
            .messages
            .first()
            .is_some_and(|m| m.content.text().starts_with("Judge the goal"))
        {
            json!({"verdict":"COMPLETED","reason":"observed result","next_input":null,"progress":{"done":true},"required_input":null}).to_string()
        } else {
            request
                .messages
                .last()
                .map(|m| m.content.text())
                .unwrap_or_default()
        };
        Ok(ChatResponse {
            content: content.into(),
            usage: Usage {
                input_tokens: 2,
                output_tokens: 2,
            },
            ..Default::default()
        })
    }
}

struct FakeExternal;
#[async_trait]
impl ExternalAgent for FakeExternal {
    async fn run(
        &self,
        request: ExternalAgentRequest,
        _: &dyn ExternalAgentEventSink,
    ) -> Result<ExternalAgentResult, AgentError> {
        if request.input.text() == "second"
            && request.session.as_ref().map(|s| s.session_id.as_str()) != Some("remote-session")
        {
            return Err(AgentError::Run("external session was not continued".into()));
        }
        Ok(ExternalAgentResult {
            content: request.input,
            structured: None,
            session: Some(ExternalSessionRef {
                session_id: "remote-session".into(),
                run_id: None,
            }),
            usage: Usage {
                input_tokens: 2,
                output_tokens: 2,
            },
        })
    }
}
#[tokio::test]
async fn workflow_dag_and_external_share_framework_goal_lifecycle() {
    let f = Fixture::new().await;
    for (loop_id, kind) in [
        ("workflow", "WORKFLOW"),
        ("dag", "WORKFLOW"),
        ("external", "EXTERNAL"),
    ] {
        let session = f.session().await;
        let goal = f
            .framework
            .goals()
            .unwrap()
            .set(GoalRecord::new(&session.id, "loop continuation"))
            .await
            .unwrap();
        let worker = f
            .framework
            .agent(AgentSelection {
                agent_loop: Some(loop_id.into()),
                ..Default::default()
            })
            .build()
            .await
            .unwrap();
        let mut request = RunRequest::new(&session.id, "first");
        request.limits.max_model_calls_per_step = Some(2);
        let result = worker
            .run_to_goal(request, &goal.id, Arc::new(Judge))
            .await
            .unwrap();
        assert!(result.output.contains("second"));
        let runs = f.framework.runs().unwrap().list(&session.id).await.unwrap();
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].kind, kind);
        assert_eq!(runs[2].kind, kind);
        assert_eq!(
            f.framework
                .goals()
                .unwrap()
                .get(&goal.id)
                .await
                .unwrap()
                .status,
            GoalStatus::Completed
        );
        if loop_id == "external" {
            assert_eq!(
                result.external_session.unwrap().session_id,
                "remote-session"
            );
        }
    }
    f.framework.close().await;
}

#[tokio::test]
async fn native_react_goal_keeps_shared_history_and_separate_checkpoint_heads() {
    let f = Fixture::new().await;
    let session = f.session().await;
    let goal = f
        .framework
        .goals()
        .unwrap()
        .set(GoalRecord::new(&session.id, "native"))
        .await
        .unwrap();
    let worker = f
        .framework
        .agent(AgentSelection {
            agent_loop: Some("react".into()),
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new(&session.id, "first");
    request.limits.max_model_calls_per_step = Some(2);
    let result = worker
        .run_to_goal(request, &goal.id, Arc::new(Judge))
        .await
        .unwrap();
    assert_eq!(result.output, "second");
    assert_eq!(f.count("ef_session_heads").await, 3);
    let snapshot = worker
        .conversation(&ConversationKey::new(&session.id))
        .await
        .unwrap();
    assert_eq!(snapshot.messages.len(), 4);
    assert_eq!(snapshot.messages[0].content.text(), "first");
    assert_eq!(snapshot.messages[2].content.text(), "second");
    let events = f
        .framework
        .events()
        .unwrap()
        .list(&session.id, 0, 1000)
        .await
        .unwrap();
    assert!(!events.iter().any(|e| e.kind == "EXECUTION_EVENT"));
    let history = f
        .framework
        .history()
        .unwrap()
        .read_session(&session.id, HistoryQuery::default())
        .await
        .unwrap();
    assert!(!history.records.messages.is_empty());
    assert!(!history.records.steps.is_empty());
    f.framework
        .sessions()
        .unwrap()
        .delete(&session.id)
        .await
        .unwrap();
    assert_eq!(f.count("ef_checkpoints").await, 0);
    assert_eq!(f.count("ef_conversations").await, 0);
    f.framework.close().await;
}

#[tokio::test]
async fn default_model_judge_runs_inside_native_budget_and_persists_decision() {
    let f = Fixture::new().await;
    let session = f.session().await;
    let goal = f
        .framework
        .goals()
        .unwrap()
        .set(GoalRecord::new(&session.id, "judge"))
        .await
        .unwrap();
    let worker = f
        .framework
        .agent(AgentSelection {
            agent_loop: Some("react".into()),
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new(&session.id, "first");
    request.limits.max_model_calls_per_step = Some(2);
    worker
        .run_to_goal(request, &goal.id, Arc::new(ModelGoalPolicy::default()))
        .await
        .unwrap();
    let goal = f.framework.goals().unwrap().get(&goal.id).await.unwrap();
    assert_eq!(goal.status, GoalStatus::Completed);
    assert_eq!(goal.runs_used, 1);
    assert_eq!(
        f.framework
            .runs()
            .unwrap()
            .list(&session.id)
            .await
            .unwrap()
            .len(),
        2
    );
    f.framework.close().await;
}

struct BadJudge;
#[async_trait]
impl GoalPolicy for BadJudge {
    async fn evaluate(&self, _: &GoalEvidence, _: &RunContext) -> Result<GoalDecision, AgentError> {
        Err(AgentError::Run("invalid verdict".into()))
    }
}

struct WaitingJudge(Arc<Notify>);
#[async_trait]
impl GoalPolicy for WaitingJudge {
    async fn evaluate(&self, _: &GoalEvidence, _: &RunContext) -> Result<GoalDecision, AgentError> {
        self.0.notify_one();
        std::future::pending().await
    }
}
#[tokio::test]
async fn cancelling_goal_future_terminates_driver_and_pauses_goal() {
    let f = Fixture::new().await;
    let session = f.session().await;
    let goals = f.framework.goals().unwrap();
    let goal = goals
        .set(GoalRecord::new(&session.id, "cancel"))
        .await
        .unwrap();
    let worker = f.worker().await;
    let started = Arc::new(Notify::new());
    let judge = Arc::new(WaitingJudge(started.clone()));
    let (goal_id, session_id) = (goal.id.clone(), session.id.clone());
    let concurrent_worker = worker.clone();
    let task = tokio::spawn(async move {
        let mut request = RunRequest::new(session_id, "first");
        request.limits.max_model_calls_per_step = Some(2);
        worker.run_to_goal(request, &goal_id, judge).await
    });
    started.notified().await;
    let mut duplicate = RunRequest::new(&session.id, "duplicate");
    duplicate.limits.max_model_calls_per_step = Some(2);
    assert!(
        concurrent_worker
            .run_to_goal(duplicate, &goal.id, Arc::new(Judge))
            .await
            .is_err()
    );
    task.abort();
    let _ = task.await;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if goals.get(&goal.id).await.unwrap().status == GoalStatus::Paused {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let runs = f.framework.runs().unwrap().list(&session.id).await.unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[1].status, ManagedRunStatus::Cancelled);
    f.framework.close().await;
}
#[tokio::test]
async fn repeated_judge_failure_and_run_limit_pause_goal() {
    let f = Fixture::new().await;
    let session = f.session().await;
    let goals = f.framework.goals().unwrap();
    let goal = goals
        .set(GoalRecord::new(&session.id, "invalid judgement"))
        .await
        .unwrap();
    let worker = f.worker().await;
    let mut request = RunRequest::new(&session.id, "first");
    request.limits.max_model_calls_per_step = Some(2);
    worker
        .run_to_goal(request.clone(), &goal.id, Arc::new(BadJudge))
        .await
        .unwrap();
    let goal = goals.get(&goal.id).await.unwrap();
    assert_eq!(goal.status, GoalStatus::Paused);
    assert_eq!(goal.consecutive_failures, 3);
    goals
        .transition(&goal.id, goal.revision, GoalStatus::Cancelled, None)
        .await
        .unwrap();
    let mut limited = GoalRecord::new(&session.id, "one round");
    limited.max_runs = 1;
    let goal = goals.set(limited).await.unwrap();
    worker
        .run_to_goal(request, &goal.id, Arc::new(Judge))
        .await
        .unwrap();
    let goal = goals.get(&goal.id).await.unwrap();
    assert_eq!(goal.status, GoalStatus::Paused);
    assert_eq!(goal.runs_used, 1);
    f.framework.close().await;
}

#[tokio::test]
async fn owned_artifacts_are_cleaned_only_after_commit_and_last_reference() {
    let f = Fixture::new().await;
    let session = f.session().await;
    let other = f.session().await;
    let sessions = f.framework.sessions().unwrap();
    let artifact = f
        .framework
        .execute(PutArtifact(vec![1, 2, 3]))
        .await
        .unwrap();
    sessions
        .attach_artifact(&session.id, artifact.id.as_str(), true)
        .await
        .unwrap();
    sessions
        .attach_artifact(&other.id, artifact.id.as_str(), false)
        .await
        .unwrap();
    sessions.delete(&session.id).await.unwrap();
    assert_eq!(f.count("ef_local_artifact_metadata").await, 1);
    assert!(
        f.framework
            .transaction(async {
                sessions.delete(&other.id).await?;
                Err::<(), _>(StateError::Conflict)
            })
            .await
            .is_err()
    );
    assert_eq!(f.count("ef_local_artifact_metadata").await, 1);
    assert!(
        f.framework
            .execute(ReadArtifact {
                id: artifact.id.clone(),
                offset: 0,
                length: 3
            })
            .await
            .is_ok()
    );
    sessions.delete(&other.id).await.unwrap();
    assert_eq!(f.count("ef_local_artifact_metadata").await, 0);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if f.count("ef_artifact_cleanup").await == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        !f._dir
            .path()
            .join("artifacts")
            .join(artifact.id.as_str())
            .exists()
    );
    f.framework.close().await;
}

#[tokio::test]
async fn shared_database_transaction_commits_and_rolls_back_business_and_framework() {
    let f = Fixture::new().await;
    let sessions = f.framework.sessions().unwrap();
    let goals = f.framework.goals().unwrap();
    let session = sessions
        .create(SessionRecord::new("outside"))
        .await
        .unwrap();
    let result = f
        .framework
        .transaction(async {
            let created = sessions.create(SessionRecord::new("inside")).await?;
            goals
                .set(GoalRecord::new(&created.id, "atomic goal"))
                .await?;
            f.database
                .call(|c| c.execute("INSERT INTO tron_links VALUES ('rollback')", []))
                .await
                .map_err(|_| StateError::Conflict)?;
            Err::<(), _>(StateError::Conflict)
        })
        .await;
    assert!(result.is_err());
    assert_eq!(f.count("ef_sessions").await, 1);
    assert_eq!(f.count("ef_goals").await, 0);
    assert_eq!(f.count("tron_links").await, 0);
    let goal = f
        .framework
        .transaction(async {
            let goal = goals.set(GoalRecord::new(&session.id, "commit")).await?;
            f.database
                .call(|c| c.execute("INSERT INTO tron_links VALUES ('commit')", []))
                .await
                .map_err(|_| StateError::Conflict)?;
            Ok(goal)
        })
        .await
        .unwrap();
    assert_eq!(goals.get(&goal.id).await.unwrap().objective, "commit");
    assert_eq!(f.count("tron_links").await, 1);
    f.framework.close().await;
    // An injected runtime remains owned and usable by the application.
    assert_eq!(f.count("tron_links").await, 1);
    assert!(sessions.get(&session.id).await.is_err());
}

#[tokio::test]
async fn artifact_remove_joins_outer_transaction_without_unlinking_on_rollback() {
    let f = Fixture::new().await;
    let artifact = f
        .framework
        .execute(PutArtifact(vec![1, 2, 3]))
        .await
        .unwrap();
    assert!(
        f.framework
            .transaction(async {
                f.framework
                    .execute(RemoveArtifact(artifact.id.clone()))
                    .await
                    .map_err(|_| StateError::Conflict)?;
                Err::<(), _>(StateError::Conflict)
            })
            .await
            .is_err()
    );
    assert!(
        f.framework
            .execute(ReadArtifact {
                id: artifact.id.clone(),
                offset: 0,
                length: 3
            })
            .await
            .is_ok()
    );
    f.framework
        .execute(RemoveArtifact(artifact.id.clone()))
        .await
        .unwrap();
    assert!(
        !f._dir
            .path()
            .join("artifacts")
            .join(artifact.id.as_str())
            .exists()
    );
    f.framework.close().await;
}

#[tokio::test]
async fn caught_inner_failure_poisoning_and_foreign_runtime_rejection() {
    let f = Fixture::new().await;
    let result = f
        .database
        .transaction(async {
            f.database
                .call(|c| c.execute("INSERT INTO tron_links VALUES ('outer')", []))
                .await
                .map_err(|_| StateError::Conflict)?;
            let inner = f
                .database
                .transaction(async {
                    f.database
                        .call(|c| c.execute("INSERT INTO tron_links VALUES ('inner')", []))
                        .await
                        .map_err(|_| StateError::Conflict)?;
                    Err::<(), _>(StateError::Conflict)
                })
                .await;
            assert!(inner.is_err());
            Ok(())
        })
        .await;
    assert!(matches!(result, Err(StateError::Conflict)));
    assert_eq!(f.count("tron_links").await, 0);
    let other = SqliteDatabase::open(f._dir.path().join("other.db"))
        .await
        .unwrap();
    assert!(
        f.database
            .transaction(async {
                assert!(
                    other
                        .call(|c| c.execute_batch("CREATE TABLE wrong(id TEXT)"))
                        .await
                        .is_err()
                );
                Ok(())
            })
            .await
            .is_err()
    );
    f.framework.close().await;
}

#[tokio::test]
async fn cancellation_rolls_back_and_unrelated_tasks_cannot_join() {
    let f = Fixture::new().await;
    let entered = Arc::new(Notify::new());
    let (database, started) = (f.database.clone(), entered.clone());
    let task = tokio::spawn(async move {
        database
            .transaction(async {
                database
                    .call(|c| c.execute("INSERT INTO tron_links VALUES ('cancel')", []))
                    .await
                    .map_err(|_| StateError::Conflict)?;
                started.notify_one();
                std::future::pending::<Result<(), StateError>>().await
            })
            .await
    });
    entered.notified().await;
    assert!(
        tokio::time::timeout(Duration::from_millis(30), f.count("tron_links"))
            .await
            .is_err()
    );
    task.abort();
    let _ = task.await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), f.count("tron_links"))
            .await
            .unwrap(),
        0
    );
    f.framework.close().await;
}

#[tokio::test]
async fn cancelled_transaction_releases_connection_without_runtime_cleanup() {
    let f = Fixture::new().await;
    let database = f.database.clone();
    let operation = database.transaction(async {
        database
            .call(|c| c.execute("INSERT INTO tron_links VALUES ('cancel')", []))
            .await
            .map_err(|_| StateError::Conflict)?;
        std::future::pending::<Result<(), StateError>>().await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(30), operation)
            .await
            .is_err()
    );
    // A synchronous caller on a current-thread runtime cannot drive tokio::spawn.
    // Rollback must have already been submitted directly to the SQLite worker.
    let count = f
        .database
        .call_blocking(|c| {
            c.query_row("SELECT count(*) FROM tron_links", [], |r| {
                r.get::<_, i64>(0)
            })
        })
        .unwrap();
    assert_eq!(count, 0);
    f.framework.close().await;
}

#[tokio::test]
async fn dropped_sql_awaiter_cannot_hide_a_late_failure_from_commit() {
    let f = Fixture::new().await;
    let finished = Arc::new(Notify::new());
    let ready = finished.clone();
    let database = f.database.clone();
    let (release, blocked) = std::sync::mpsc::channel();
    let task = tokio::spawn(async move {
        database.transaction(async {
            database.call(|c|c.execute("INSERT INTO tron_links VALUES ('outer')",[])).await.map_err(|_|StateError::Conflict)?;
            let (entered,started)=tokio::sync::oneshot::channel();
            let mut pending=Box::pin(database.call(move |c| {
                let _=entered.send(());
                blocked.recv_timeout(Duration::from_secs(5)).unwrap();
                c.execute("INSERT INTO tron_links VALUES ('outer')",[])
            }));
            tokio::select! { _=started=>{}, _=&mut pending=>panic!("worker must remain blocked") }
            drop(pending);
            ready.notify_one();
            Ok(())
        }).await
    });
    finished.notified().await;
    release.send(()).unwrap();
    assert!(matches!(task.await.unwrap(), Err(StateError::Conflict)));
    assert_eq!(f.count("tron_links").await, 0);
    f.framework.close().await;
}

#[tokio::test]
async fn goal_continuation_has_one_session_separate_scopes_and_durable_lineage() {
    let f = Fixture::new().await;
    let session = f.session().await;
    let goals = f.framework.goals().unwrap();
    let goal = goals
        .set(GoalRecord::new(&session.id, "two executions"))
        .await
        .unwrap();
    let worker = f.worker().await;
    let mut request = RunRequest::new(&session.id, "first");
    request.limits.max_model_calls_per_step = Some(2);
    let result = worker
        .run_to_goal(request, &goal.id, Arc::new(Judge))
        .await
        .unwrap();
    assert_eq!(result.output, "second");
    let goal = goals.get(&goal.id).await.unwrap();
    assert_eq!(goal.status, GoalStatus::Completed);
    assert_eq!(goal.runs_used, 2);
    let runs = f.framework.runs().unwrap().list(&session.id).await.unwrap();
    assert_eq!(runs.len(), 3);
    assert!(
        runs.iter()
            .all(|r| r.session_id == session.id && r.status == ManagedRunStatus::Completed)
    );
    let first = &runs[0];
    let driver = &runs[1];
    let child = &runs[2];
    assert_eq!(first.execution_scope, "main");
    assert_ne!(child.execution_scope, driver.execution_scope);
    assert_eq!(driver.parent_run_id, Some(first.id));
    assert_eq!(child.parent_run_id, Some(driver.id));
    assert_eq!(child.root_run_id, first.id);
    let events = f
        .framework
        .events()
        .unwrap()
        .list(&session.id, 0, 1000)
        .await
        .unwrap();
    assert_eq!(events.iter().filter(|e| e.kind == "GOAL_JUDGED").count(), 2);
    assert!(!events.iter().any(|e| e.kind == "EXECUTION_EVENT"));
    let history = f
        .framework
        .history()
        .unwrap()
        .read_session(&session.id, HistoryQuery::default())
        .await
        .unwrap();
    assert_eq!(history.runs.len(), 3);
    assert!(events.windows(2).all(|w| w[0].sequence < w[1].sequence));
    // Completed goal history remains while the session can receive a new goal.
    goals
        .set(GoalRecord::new(&session.id, "next goal"))
        .await
        .unwrap();
    assert_eq!(goals.list(&session.id).await.unwrap().len(), 2);
    f.framework.close().await;
}

#[tokio::test]
async fn goal_cas_and_context_cas_reject_stale_updates() {
    let f = Fixture::new().await;
    let session = f.session().await;
    let sessions = f.framework.sessions().unwrap();
    let goals = f.framework.goals().unwrap();
    let mut goal = goals
        .set(GoalRecord::new(&session.id, "initial"))
        .await
        .unwrap();
    assert!(
        goals
            .set(GoalRecord::new(&session.id, "duplicate"))
            .await
            .is_err()
    );
    let mut paused = goals
        .transition(&goal.id, goal.revision, GoalStatus::Paused, None)
        .await
        .unwrap();
    assert!(goals.update(goal.clone(), goal.revision).await.is_err());
    // Updating objective cannot overwrite judgement counters or goal status.
    paused.objective = "updated".into();
    paused.status = GoalStatus::Active;
    paused.runs_used = 42;
    goal = goals.update(paused.clone(), paused.revision).await.unwrap();
    assert_eq!(goal.status, GoalStatus::Paused);
    assert_eq!(goal.runs_used, 0);
    assert!(
        goals
            .decide(
                &goal.id,
                goal.revision,
                RunId::new_v4(),
                GoalDecision {
                    verdict: GoalVerdict::Completed,
                    reason: "fake".into(),
                    next_input: None,
                    progress: json!(null),
                    required_input: None
                }
            )
            .await
            .is_err()
    );
    assert_eq!(
        sessions
            .write_context(&session.id, "key", None, json!(1))
            .await
            .unwrap(),
        0
    );
    assert!(
        sessions
            .write_context(&session.id, "key", None, json!(2))
            .await
            .is_err()
    );
    assert_eq!(
        sessions
            .write_context(&session.id, "key", Some(0), json!(2))
            .await
            .unwrap(),
        1
    );
    f.framework.close().await;
}

#[tokio::test]
async fn deleting_session_is_atomic_and_tombstones_late_writes() {
    let f = Fixture::new().await;
    let sessions = f.framework.sessions().unwrap();
    let session = f.session().await;
    f.framework
        .goals()
        .unwrap()
        .set(GoalRecord::new(&session.id, "delete everything"))
        .await
        .unwrap();
    let worker = f.worker().await;
    worker
        .run(RunRequest::new(&session.id, "done"))
        .await
        .unwrap();
    sessions
        .write_context(&session.id, "key", None, json!({"value":1}))
        .await
        .unwrap();
    sessions.archive(&session.id).await.unwrap();
    let result = f
        .framework
        .transaction(async {
            assert!(sessions.delete(&session.id).await?);
            Err::<(), _>(StateError::Conflict)
        })
        .await;
    assert!(result.is_err());
    assert!(sessions.get(&session.id).await.is_ok());
    assert!(sessions.delete(&session.id).await.unwrap());
    assert!(!sessions.delete(&session.id).await.unwrap());
    for table in [
        "ef_sessions",
        "ef_goals",
        "ef_runs",
        "ef_events",
        "ef_session_context",
    ] {
        assert_eq!(f.count(table).await, 0);
    }
    let mut reuse = SessionRecord::new("reuse");
    reuse.id = session.id.clone();
    assert!(sessions.create(reuse).await.is_err());
    assert!(
        worker
            .run(RunRequest::new(&session.id, "late"))
            .await
            .is_err()
    );
    assert!(
        worker
            .append_conversation(
                &ConversationKey::new(&session.id),
                0,
                vec![Message::user("late")]
            )
            .await
            .is_err()
    );
    f.framework.close().await;
}

fn running(session: &str, scope: &str, goal: Option<String>) -> ManagedRun {
    let id = RunId::new_v4();
    ManagedRun {
        id,
        session_id: session.into(),
        execution_scope: scope.into(),
        conversation_branch: "main".into(),
        parent_run_id: None,
        root_run_id: id,
        goal_id: goal,
        kind: "EXTERNAL".into(),
        status: ManagedRunStatus::Running,
        result: None,
        error: None,
        started_at_ms: lifecycle_time_ms(),
        finished_at_ms: None,
        lease_until_ms: lifecycle_time_ms() + 60_000,
    }
}

#[derive(Clone, Copy)]
enum RecoveryCheckpointLease {
    Expired,
    Live,
    Released,
    PreviousAttempt,
}

async fn checkpoint_recovery_fixture(
    lease: RecoveryCheckpointLease,
) -> (Fixture, SessionRecord, ManagedRun) {
    let f = Fixture::new().await;
    let session = f.session().await;
    let now = lifecycle_time_ms();
    let mut run = running(&session.id, "main", None);
    run.kind = "WORKFLOW".into();
    run.started_at_ms = if matches!(lease, RecoveryCheckpointLease::PreviousAttempt) {
        now
    } else {
        now - 60_000
    };
    run.lease_until_ms = now + 8 * 60 * 60 * 1000;
    let services = f.framework.services().unwrap();
    services
        .get::<SessionLifecycleStoreService>()
        .unwrap()
        .0
        .register_run(run.clone(), false)
        .await
        .unwrap();
    let state = services.get::<StateStoreService>().unwrap().0.clone();
    let mut cp = Checkpoint::new(run.id, &session.id, "fixture", 1, json!({}));
    let mut message = HistoryMessage::user(run.id, &"partial output".into());
    message.role = Role::Assistant;
    message.status = HistoryStatus::Running;
    cp.history.messages.push(message);
    state.create(cp, None).await.unwrap();
    let owner = state
        .claim(run.id, RunId::new_v4(), Duration::from_secs(60))
        .await
        .unwrap();
    if matches!(lease, RecoveryCheckpointLease::Released) {
        state.release(&owner).await.unwrap();
    }
    if !matches!(lease, RecoveryCheckpointLease::Live) {
        let id = run.id.to_string();
        f.database
            .call(move |c| {
                c.execute(
                    "UPDATE ef_checkpoints SET expires=?2 WHERE run_id=?1",
                    rusqlite::params![id, now - 1_000],
                )
            })
            .await
            .unwrap();
    }
    (f, session, run)
}

#[tokio::test]
async fn expired_checkpoint_recovers_long_deadline_and_allows_archive() {
    let (f, session, run) = checkpoint_recovery_fixture(RecoveryCheckpointLease::Expired).await;
    let lifecycle = f
        .framework
        .services()
        .unwrap()
        .get::<SessionLifecycleStoreService>()
        .unwrap()
        .0
        .clone();
    assert_eq!(lifecycle.recover_expired_runs().await.unwrap(), 1);
    assert_eq!(lifecycle.recover_expired_runs().await.unwrap(), 0);
    assert_eq!(
        lifecycle.get_run(run.id).await.unwrap().status,
        ManagedRunStatus::RecoveryRequired
    );
    let history = f
        .framework
        .history()
        .unwrap()
        .read_session(&session.id, Default::default())
        .await
        .unwrap();
    assert!(history.active_runs.is_empty());
    assert_eq!(
        history.records.messages[0].status,
        HistoryStatus::Interrupted
    );
    assert_eq!(
        history.records.messages[0].content[0],
        HistoryContentBlock::Text {
            text: "partial output".into(),
        }
    );
    f.framework
        .sessions()
        .unwrap()
        .archive(&session.id)
        .await
        .unwrap();
    f.framework.close().await;
}

#[tokio::test]
async fn recovery_preserves_live_finishing_and_newly_admitted_attempts() {
    for lease in [
        RecoveryCheckpointLease::Live,
        RecoveryCheckpointLease::Released,
        RecoveryCheckpointLease::PreviousAttempt,
    ] {
        let (f, session, run) = checkpoint_recovery_fixture(lease).await;
        let lifecycle = f
            .framework
            .services()
            .unwrap()
            .get::<SessionLifecycleStoreService>()
            .unwrap()
            .0
            .clone();
        assert_eq!(lifecycle.recover_expired_runs().await.unwrap(), 0);
        assert_eq!(
            lifecycle.get_run(run.id).await.unwrap().status,
            ManagedRunStatus::Running
        );
        assert!(matches!(
            f.framework.sessions().unwrap().archive(&session.id).await,
            Err(StateError::Busy)
        ));
        f.framework.close().await;
    }
}

#[tokio::test]
async fn plugin_startup_recovers_expired_checkpoint_before_session_archive() {
    let (f, session, run) = checkpoint_recovery_fixture(RecoveryCheckpointLease::Expired).await;
    let path = f._dir.path().join("shared.db");
    let artifacts = f._dir.path().join("artifacts");
    f.framework.close().await;
    let reopened = Framework::builder()
        .default_models(false)
        .plugin(LocalStoragePlugin::new(path, artifacts))
        .build()
        .await
        .unwrap();
    assert_eq!(
        reopened
            .services()
            .unwrap()
            .get::<SessionLifecycleStoreService>()
            .unwrap()
            .0
            .get_run(run.id)
            .await
            .unwrap()
            .status,
        ManagedRunStatus::RecoveryRequired
    );
    reopened
        .sessions()
        .unwrap()
        .archive(&session.id)
        .await
        .unwrap();
    reopened.close().await;
}

#[tokio::test]
async fn archive_recovers_checkpoint_that_expired_after_plugin_startup() {
    let (f, session, run) = checkpoint_recovery_fixture(RecoveryCheckpointLease::Expired).await;
    f.framework
        .sessions()
        .unwrap()
        .archive(&session.id)
        .await
        .unwrap();
    assert_eq!(
        f.framework
            .services()
            .unwrap()
            .get::<SessionLifecycleStoreService>()
            .unwrap()
            .0
            .get_run(run.id)
            .await
            .unwrap()
            .status,
        ManagedRunStatus::RecoveryRequired
    );
    f.framework.close().await;
}

#[tokio::test]
async fn deadline_recovery_also_supports_lifecycle_only_storage() {
    let dir = tempfile::tempdir().unwrap();
    let database = SqliteDatabase::open(dir.path().join("lifecycle.db"))
        .await
        .unwrap();
    let store = SqliteLifecycleStore::with_database(database).await.unwrap();
    let session = store
        .create_session(SessionRecord::new("lifecycle-only"))
        .await
        .unwrap();
    let mut run = running(&session.id, "main", None);
    run.lease_until_ms = 0;
    store.register_run(run.clone(), false).await.unwrap();
    assert_eq!(store.recover_expired_runs().await.unwrap(), 1);
    assert_eq!(
        store.get_run(run.id).await.unwrap().status,
        ManagedRunStatus::RecoveryRequired
    );
}

#[tokio::test]
async fn expired_child_recovery_pauses_owner_goal_without_replaying() {
    let f = Fixture::new().await;
    let owner = f.session().await;
    let child = f.session().await;
    let goal = f
        .framework
        .goals()
        .unwrap()
        .set(GoalRecord::new(&owner.id, "recover"))
        .await
        .unwrap();
    let lifecycle = f
        .framework
        .services()
        .unwrap()
        .get::<SessionLifecycleStoreService>()
        .unwrap()
        .0
        .clone();
    let mut parent = running(&owner.id, "main", Some(goal.id.clone()));
    lifecycle.register_run(parent.clone(), false).await.unwrap();
    assert!(
        lifecycle
            .register_run(running(&owner.id, "main", None), false)
            .await
            .is_err()
    );
    assert!(matches!(
        f.framework.sessions().unwrap().archive(&owner.id).await,
        Err(StateError::Busy)
    ));
    let mut run = running(&child.id, "external", Some(goal.id.clone()));
    run.parent_run_id = Some(parent.id);
    run.root_run_id = parent.id;
    run.lease_until_ms = 0;
    lifecycle.register_run(run.clone(), false).await.unwrap();
    parent.status = ManagedRunStatus::Completed;
    lifecycle.finish_run(parent).await.unwrap();
    assert_eq!(lifecycle.recover_expired_runs().await.unwrap(), 1);
    assert_eq!(
        lifecycle.get_run(run.id).await.unwrap().status,
        ManagedRunStatus::RecoveryRequired
    );
    assert_eq!(
        f.framework
            .goals()
            .unwrap()
            .get(&goal.id)
            .await
            .unwrap()
            .status,
        GoalStatus::Paused
    );
    f.framework
        .sessions()
        .unwrap()
        .archive(&owner.id)
        .await
        .unwrap();
    assert!(
        f.framework
            .sessions()
            .unwrap()
            .delete(&owner.id)
            .await
            .unwrap()
    );
    assert!(matches!(
        f.framework.sessions().unwrap().get(&child.id).await,
        Err(StateError::NotFound)
    ));
    assert_eq!(f.count("ef_runs").await, 0);
    f.framework.close().await;
}

#[tokio::test]
async fn archived_history_read_does_not_create_or_touch_conversations() {
    let f = Fixture::new().await;
    let session = f.session().await;
    let agent = f
        .framework
        .agent(AgentSelection {
            agent_loop: Some("react".into()),
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    agent
        .run(RunRequest::new(&session.id, "preserve this history"))
        .await
        .unwrap();
    f.framework
        .sessions()
        .unwrap()
        .archive(&session.id)
        .await
        .unwrap();
    let conversations = f
        .framework
        .services()
        .unwrap()
        .get::<ConversationStoreService>()
        .unwrap()
        .0
        .clone();
    let before = conversations.history(&session.id).await.unwrap();
    let record = f
        .framework
        .sessions()
        .unwrap()
        .get(&session.id)
        .await
        .unwrap();
    let again = conversations.history(&session.id).await.unwrap();
    assert_eq!(
        serde_json::to_value(&before).unwrap(),
        serde_json::to_value(&again).unwrap()
    );
    assert_eq!(before[0].messages.len(), 2);
    assert_eq!(
        f.framework
            .sessions()
            .unwrap()
            .get(&session.id)
            .await
            .unwrap()
            .revision,
        record.revision
    );
    let count = f.count("ef_conversations").await;
    assert!(
        conversations
            .history("missing-session")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(f.count("ef_conversations").await, count);
    f.framework.close().await;
}
#[tokio::test]
async fn stable_event_identity_is_idempotent_and_rejects_different_payload() {
    let f = Fixture::new().await;
    let session = f.session().await;
    let store = f.framework.sessions().unwrap().store;
    let event = SessionEvent::new(&session.id, "tron.presentation", json!({"text":"same"}));
    let first = store.append_event(event.clone()).await.unwrap();
    let repeated = store.append_event(event.clone()).await.unwrap();
    assert_eq!(first.sequence, repeated.sequence);
    let mut collision = event;
    collision.payload = json!({"text":"different"});
    assert!(matches!(
        store.append_event(collision).await,
        Err(StateError::Conflict)
    ));
    f.framework.close().await;
}
#[tokio::test]
async fn terminal_goal_can_be_cleared_without_resuming_execution() {
    let f = Fixture::new().await;
    let session = f.session().await;
    let goals = f.framework.goals().unwrap();
    let goal = goals
        .set(GoalRecord::new(&session.id, "cancelled objective"))
        .await
        .unwrap();
    let cancelled = goals
        .transition(&goal.id, goal.revision, GoalStatus::Cancelled, None)
        .await
        .unwrap();
    assert!(
        goals
            .transition(&goal.id, cancelled.revision, GoalStatus::Active, None)
            .await
            .is_err()
    );
    let cleared = goals
        .transition(&goal.id, cancelled.revision, GoalStatus::Cleared, None)
        .await
        .unwrap();
    assert_eq!(cleared.status, GoalStatus::Cleared);
    assert!(goals.active(&session.id).await.unwrap().is_none());
    f.framework.close().await;
}

#[tokio::test]
async fn repeated_synchronous_sql_does_not_exhaust_tokio_task_budget() {
    let f = Fixture::new().await;
    for i in 0..512 {
        f.database
            .call_blocking(move |c| {
                c.execute("INSERT INTO tron_links VALUES (?1)", [i.to_string()])
            })
            .unwrap();
    }
    assert_eq!(f.count("tron_links").await, 512);
    f.framework.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn synchronous_waiters_do_not_starve_the_native_transaction_runtime() {
    let f = Fixture::new().await;
    let database = f.database.clone();
    let (ready, waiting) = tokio::sync::oneshot::channel();
    let (release, resume) = tokio::sync::oneshot::channel();
    let transaction = tokio::spawn(async move {
        database
            .transaction(async {
                ready.send(()).unwrap();
                resume.await.unwrap();
                database
                    .call(|c| c.execute("INSERT INTO tron_links VALUES ('native')", []))
                    .await
                    .map_err(|e| StateError::Backend(e.to_string()))?;
                Ok(())
            })
            .await
    });
    waiting.await.unwrap();
    let waiters = (0..2)
        .map(|index| {
            let database = f.database.clone();
            tokio::spawn(async move {
                database
                    .call_blocking(move |c| {
                        c.execute("INSERT INTO tron_links VALUES (?1)", [index.to_string()])
                    })
                    .unwrap();
            })
        })
        .collect::<Vec<_>>();
    tokio::spawn(async move {
        tokio::task::yield_now().await;
        release.send(()).unwrap();
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        transaction.await.unwrap().unwrap();
        for waiter in waiters {
            waiter.await.unwrap();
        }
    })
    .await
    .expect("synchronous SQL must allow the native transaction task to resume");
    assert_eq!(f.count("tron_links").await, 3);
    f.framework.close().await;
}

#[tokio::test]
async fn missing_session_lookup_allows_creation_in_shared_transaction() {
    let directory = tempfile::tempdir().unwrap();
    let database = SqliteDatabase::open(directory.path().join("lookup-create.sqlite"))
        .await
        .unwrap();
    let lifecycle = SqliteLifecycleStore::with_database(database.clone())
        .await
        .unwrap();
    database
        .transaction(async {
            assert!(matches!(
                lifecycle.get_session("delegated-child").await,
                Err(StateError::NotFound)
            ));
            let mut record = SessionRecord::new("Delegated child");
            record.id = "delegated-child".into();
            lifecycle.create_session(record).await?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        lifecycle
            .get_session("delegated-child")
            .await
            .unwrap()
            .title,
        "Delegated child"
    );
}
