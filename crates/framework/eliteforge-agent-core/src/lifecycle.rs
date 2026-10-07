//! Framework-owned session, goal, execution and durable-event lifecycle.
use crate::*;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

pub fn lifecycle_time_ms() -> u64 {
    unix_time_ms()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SessionStatus {
    Active,
    Archived,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: String,
    pub title: String,
    pub status: SessionStatus,
    pub revision: u64,
    pub metadata: Value,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}
impl SessionRecord {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            id: RunId::new_v4().to_string(),
            title: title.into(),
            status: SessionStatus::Active,
            revision: 0,
            metadata: Value::Null,
            created_at_ms: lifecycle_time_ms(),
            updated_at_ms: lifecycle_time_ms(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum GoalStatus {
    Active,
    Paused,
    Blocked,
    Completed,
    Cancelled,
    Cleared,
}
impl GoalStatus {
    pub fn is_open(self) -> bool {
        matches!(self, Self::Active | Self::Paused | Self::Blocked)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalCriterion {
    pub id: String,
    pub text: String,
    pub completed: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalRecord {
    pub id: String,
    pub session_id: String,
    pub objective: String,
    pub status: GoalStatus,
    pub revision: u64,
    pub runs_used: u32,
    pub max_runs: u32,
    pub criteria: Vec<GoalCriterion>,
    pub progress: Value,
    pub last_run_id: Option<RunId>,
    pub last_decision: Option<GoalDecision>,
    pub consecutive_failures: u32,
    pub required_input: Option<String>,
    pub reason: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}
impl GoalRecord {
    pub fn new(session_id: impl Into<String>, objective: impl Into<String>) -> Self {
        Self {
            id: RunId::new_v4().to_string(),
            session_id: session_id.into(),
            objective: objective.into(),
            status: GoalStatus::Active,
            revision: 0,
            runs_used: 0,
            max_runs: 20,
            criteria: vec![],
            progress: Value::Null,
            last_run_id: None,
            last_decision: None,
            consecutive_failures: 0,
            required_input: None,
            reason: None,
            created_at_ms: lifecycle_time_ms(),
            updated_at_ms: lifecycle_time_ms(),
        }
    }
    pub fn validate(&self) -> Result<(), StateError> {
        if self.id.trim().is_empty()
            || self.session_id.trim().is_empty()
            || self.objective.trim().is_empty()
            || self.objective.chars().count() > 8000
            || self.max_runs == 0
        {
            return Err(StateError::Invalid("goal requires an identity, objective of at most 8000 characters and a positive run limit".into()));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum GoalVerdict {
    Continue,
    Completed,
    Blocked,
    NeedsUserInput,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoalDecision {
    pub verdict: GoalVerdict,
    pub reason: String,
    pub next_input: Option<String>,
    #[serde(default)]
    pub progress: Value,
    #[serde(default)]
    pub required_input: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalEvidence {
    pub goal: GoalRecord,
    pub run_id: RunId,
    pub output: Content,
    pub steps: usize,
}
#[async_trait]
pub trait GoalPolicy: Send + Sync {
    async fn evaluate(
        &self,
        evidence: &GoalEvidence,
        parent: &RunContext,
    ) -> Result<GoalDecision, AgentError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ManagedRunStatus {
    Running,
    Completed,
    AwaitingInput,
    Exhausted,
    Failed,
    Cancelled,
    RecoveryRequired,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedRun {
    pub id: RunId,
    pub session_id: String,
    pub execution_scope: String,
    pub conversation_branch: String,
    pub parent_run_id: Option<RunId>,
    pub root_run_id: RunId,
    pub goal_id: Option<String>,
    pub kind: String,
    pub status: ManagedRunStatus,
    pub result: Option<RunResult>,
    pub error: Option<String>,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub lease_until_ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEvent {
    pub id: String,
    pub session_id: String,
    pub run_id: Option<RunId>,
    pub goal_id: Option<String>,
    pub sequence: u64,
    pub kind: String,
    pub payload: Value,
    pub created_at_ms: u64,
}
impl SessionEvent {
    pub fn new(session_id: impl Into<String>, kind: impl Into<String>, payload: Value) -> Self {
        Self {
            id: RunId::new_v4().to_string(),
            session_id: session_id.into(),
            run_id: None,
            goal_id: None,
            sequence: 0,
            kind: kind.into(),
            payload,
            created_at_ms: lifecycle_time_ms(),
        }
    }
}

/// Each mutation and its event are committed atomically by the storage adapter.
#[crate::extension_point(single, adapter = crate::SessionLifecyclePlugin)]
#[async_trait]
pub trait SessionLifecycleStore: Send + Sync {
    async fn create_session(&self, session: SessionRecord) -> Result<SessionRecord, StateError>;
    async fn get_session(&self, id: &str) -> Result<SessionRecord, StateError>;
    async fn list_sessions(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SessionRecord>, StateError>;
    async fn update_session(
        &self,
        session: SessionRecord,
        expected: u64,
    ) -> Result<SessionRecord, StateError>;
    async fn create_goal(&self, goal: GoalRecord) -> Result<GoalRecord, StateError>;
    async fn get_goal(&self, id: &str) -> Result<GoalRecord, StateError>;
    async fn active_goal(&self, session: &str) -> Result<Option<GoalRecord>, StateError>;
    async fn list_goals(&self, session: &str) -> Result<Vec<GoalRecord>, StateError>;
    async fn update_goal(
        &self,
        goal: GoalRecord,
        expected: u64,
        event: SessionEvent,
    ) -> Result<GoalRecord, StateError>;
    async fn register_run(&self, run: ManagedRun, resume: bool) -> Result<(), StateError>;
    async fn finish_run(&self, run: ManagedRun) -> Result<(), StateError>;
    async fn get_run(&self, id: RunId) -> Result<ManagedRun, StateError>;
    async fn list_runs(&self, session: &str) -> Result<Vec<ManagedRun>, StateError>;
    async fn append_event(&self, event: SessionEvent) -> Result<SessionEvent, StateError>;
    async fn list_events(
        &self,
        session: &str,
        after: u64,
        limit: usize,
    ) -> Result<Vec<SessionEvent>, StateError>;
    async fn recover_expired_runs(&self) -> Result<usize, StateError>;
    async fn read_context(
        &self,
        session: &str,
        key: &str,
    ) -> Result<Option<(u64, Value)>, StateError>;
    async fn write_context(
        &self,
        session: &str,
        key: &str,
        expected: Option<u64>,
        value: Value,
    ) -> Result<u64, StateError>;
    async fn attach_artifact(
        &self,
        session: &str,
        artifact: &str,
        owned: bool,
    ) -> Result<(), StateError>;
}
pub struct SessionLifecycleStoreService(pub Arc<dyn SessionLifecycleStore>);
pub struct SessionLifecyclePlugin(pub Arc<dyn SessionLifecycleStore>);
impl SessionLifecyclePlugin {
    pub fn from_arc(store: Arc<dyn SessionLifecycleStore>) -> Self {
        Self(store)
    }
}
#[async_trait]
impl Plugin for SessionLifecyclePlugin {
    fn id(&self) -> &str {
        "framework.session-lifecycle"
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        context.provide(SessionLifecycleStoreService(self.0.clone()))?;
        context.contribute_extension(self.0.clone())
    }
}

#[derive(Clone)]
pub struct SessionService {
    pub store: Arc<dyn SessionLifecycleStore>,
    pub deletion: Arc<dyn SessionStore>,
}
impl SessionService {
    pub async fn create(&self, session: SessionRecord) -> Result<SessionRecord, StateError> {
        self.store.create_session(session).await
    }
    pub async fn get(&self, id: &str) -> Result<SessionRecord, StateError> {
        self.store.get_session(id).await
    }
    pub async fn list(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SessionRecord>, StateError> {
        self.store.list_sessions(after, limit).await
    }
    pub async fn update(
        &self,
        session: SessionRecord,
        expected: u64,
    ) -> Result<SessionRecord, StateError> {
        self.store.update_session(session, expected).await
    }
    pub async fn archive(&self, id: &str) -> Result<SessionRecord, StateError> {
        let mut session = self.get(id).await?;
        let revision = session.revision;
        session.status = SessionStatus::Archived;
        self.update(session, revision).await
    }
    pub async fn restore(&self, id: &str) -> Result<SessionRecord, StateError> {
        let mut session = self.get(id).await?;
        let revision = session.revision;
        session.status = SessionStatus::Active;
        self.update(session, revision).await
    }
    pub async fn delete(&self, id: &str) -> Result<bool, StateError> {
        self.deletion.delete_session(id).await
    }
    pub async fn read_context(
        &self,
        id: &str,
        key: &str,
    ) -> Result<Option<(u64, Value)>, StateError> {
        self.store.read_context(id, key).await
    }
    pub async fn write_context(
        &self,
        id: &str,
        key: &str,
        expected: Option<u64>,
        value: Value,
    ) -> Result<u64, StateError> {
        self.store.write_context(id, key, expected, value).await
    }
    pub async fn attach_artifact(
        &self,
        id: &str,
        artifact: &str,
        owned: bool,
    ) -> Result<(), StateError> {
        self.store.attach_artifact(id, artifact, owned).await
    }
}

#[derive(Clone, Default)]
#[doc(hidden)]
pub struct SessionPersistenceFailure(pub Arc<std::sync::Mutex<Option<StateError>>>);
#[derive(Clone)]
pub struct GoalService(pub Arc<dyn SessionLifecycleStore>);
impl GoalService {
    pub async fn set(&self, goal: GoalRecord) -> Result<GoalRecord, StateError> {
        goal.validate()?;
        self.0.create_goal(goal).await
    }
    pub async fn get(&self, id: &str) -> Result<GoalRecord, StateError> {
        self.0.get_goal(id).await
    }
    pub async fn active(&self, session: &str) -> Result<Option<GoalRecord>, StateError> {
        self.0.active_goal(session).await
    }
    pub async fn list(&self, session: &str) -> Result<Vec<GoalRecord>, StateError> {
        self.0.list_goals(session).await
    }
    pub async fn update(&self, goal: GoalRecord, expected: u64) -> Result<GoalRecord, StateError> {
        goal.validate()?;
        let mut prior = self.get(&goal.id).await?;
        if !prior.status.is_open()
            || prior.revision != expected
            || goal.revision != expected
            || goal.session_id != prior.session_id
        {
            return Err(StateError::Conflict);
        }
        prior.objective = goal.objective;
        prior.max_runs = goal.max_runs;
        prior.criteria = goal.criteria;
        if prior.status == GoalStatus::Active && prior.runs_used >= prior.max_runs {
            prior.status = GoalStatus::Paused;
            prior.reason = Some("run_limit".into());
        }
        self.save(prior, expected, "GOAL_UPDATED", Value::Null)
            .await
    }
    async fn save(
        &self,
        goal: GoalRecord,
        expected: u64,
        kind: &str,
        payload: Value,
    ) -> Result<GoalRecord, StateError> {
        let mut event = SessionEvent::new(&goal.session_id, kind, payload);
        event.goal_id = Some(goal.id.clone());
        event.run_id = goal.last_run_id;
        self.0.update_goal(goal, expected, event).await
    }
    pub async fn transition(
        &self,
        id: &str,
        expected: u64,
        status: GoalStatus,
        reason: Option<String>,
    ) -> Result<GoalRecord, StateError> {
        let mut goal = self.get(id).await?;
        if !goal.status.is_open() || goal.revision != expected || status == GoalStatus::Completed {
            return Err(StateError::Conflict);
        }
        if status == GoalStatus::Active && goal.runs_used >= goal.max_runs {
            return Err(StateError::Conflict);
        }
        if status == GoalStatus::Active {
            goal.consecutive_failures = 0;
        }
        goal.status = status;
        goal.reason = reason;
        goal.required_input = None;
        self.save(
            goal,
            expected,
            "GOAL_STATE_CHANGED",
            serde_json::to_value(status).map_err(|e| StateError::Invalid(e.to_string()))?,
        )
        .await
    }
    pub async fn decide(
        &self,
        id: &str,
        expected: u64,
        run: RunId,
        decision: GoalDecision,
    ) -> Result<GoalRecord, StateError> {
        let mut goal = self.get(id).await?;
        if goal.revision != expected || goal.status != GoalStatus::Active {
            return Err(StateError::Conflict);
        }
        if goal.last_run_id == Some(run) {
            return Err(StateError::Conflict);
        }
        if decision.reason.trim().is_empty()
            || (decision.verdict == GoalVerdict::Continue
                && decision
                    .next_input
                    .as_ref()
                    .is_none_or(|p| p.trim().is_empty()))
        {
            return Err(StateError::Invalid(
                "goal verdict needs a reason and continuation needs input".into(),
            ));
        }
        let no_progress = goal.last_decision.as_ref().is_some_and(|last| {
            last.next_input == decision.next_input && last.progress == decision.progress
        });
        goal.runs_used = goal.runs_used.checked_add(1).ok_or(StateError::Conflict)?;
        goal.status = match decision.verdict {
            GoalVerdict::Completed => GoalStatus::Completed,
            GoalVerdict::Blocked | GoalVerdict::NeedsUserInput => GoalStatus::Blocked,
            GoalVerdict::Continue if no_progress || goal.runs_used >= goal.max_runs => {
                GoalStatus::Paused
            }
            GoalVerdict::Continue => GoalStatus::Active,
        };
        goal.reason = Some(if no_progress && goal.status == GoalStatus::Paused {
            "no_progress".into()
        } else if goal.status == GoalStatus::Paused {
            "run_limit".into()
        } else {
            decision.reason.clone()
        });
        goal.required_input = decision.required_input.clone();
        goal.progress = decision.progress.clone();
        goal.last_run_id = Some(run);
        goal.last_decision = Some(decision.clone());
        goal.consecutive_failures = 0;
        self.save(
            goal,
            expected,
            "GOAL_JUDGED",
            serde_json::to_value(decision).map_err(|e| StateError::Invalid(e.to_string()))?,
        )
        .await
    }
    pub async fn judgement_failed(
        &self,
        id: &str,
        expected: u64,
        reason: String,
    ) -> Result<GoalRecord, StateError> {
        let mut goal = self.get(id).await?;
        if goal.status != GoalStatus::Active || goal.revision != expected {
            return Err(StateError::Conflict);
        }
        goal.consecutive_failures = goal.consecutive_failures.saturating_add(1);
        if goal.consecutive_failures >= 3 {
            goal.status = GoalStatus::Paused;
        }
        goal.reason = Some(reason.clone());
        self.save(goal, expected, "GOAL_JUDGE_FAILED", Value::String(reason))
            .await
    }
}
#[derive(Clone)]
pub struct RunQueryService(pub Arc<dyn SessionLifecycleStore>);
impl RunQueryService {
    pub async fn get(&self, id: RunId) -> Result<ManagedRun, StateError> {
        self.0.get_run(id).await
    }
    pub async fn list(&self, session: &str) -> Result<Vec<ManagedRun>, StateError> {
        self.0.list_runs(session).await
    }
}
#[derive(Clone)]
pub struct EventQueryService(pub Arc<dyn SessionLifecycleStore>);
impl EventQueryService {
    pub async fn list(
        &self,
        session: &str,
        after: u64,
        limit: usize,
    ) -> Result<Vec<SessionEvent>, StateError> {
        self.0.list_events(session, after, limit).await
    }
}
