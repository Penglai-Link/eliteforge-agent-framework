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
    assert!(events.iter().any(|e| e.kind == "EXECUTION_EVENT"));
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
    assert!(events.iter().any(|e| e.kind == "EXECUTION_EVENT"));
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
