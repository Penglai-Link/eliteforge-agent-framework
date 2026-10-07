use crate::{SqliteDatabase, database::begin};
use async_trait::async_trait;
use eliteforge_agent_core::*;
use rusqlite::{OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

#[derive(Clone)]
pub struct SqliteLifecycleStore {
    database: SqliteDatabase,
    closed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

pub(crate) fn sql(error: rusqlite::Error) -> StateError {
    match error {
        rusqlite::Error::SqliteFailure(f, _)
            if f.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            StateError::Conflict
        }
        _ => StateError::Backend("session lifecycle SQLite operation failed".into()),
    }
}
fn worker(error: tokio_rusqlite::Error<StateError>) -> StateError {
    match error {
        tokio_rusqlite::Error::Error(e) => e,
        _ => StateError::Backend("session lifecycle worker unavailable".into()),
    }
}
fn encode(value: &impl Serialize) -> Result<String, StateError> {
    serde_json::to_string(value).map_err(|e| StateError::Invalid(e.to_string()))
}
fn decode<T: DeserializeOwned>(value: String) -> Result<T, StateError> {
    serde_json::from_str(&value).map_err(|_| StateError::Invalid("corrupt lifecycle record".into()))
}
fn status(value: &impl Serialize) -> Result<String, StateError> {
    Ok(encode(value)?.trim_matches('"').to_owned())
}

pub(crate) fn initialize(c: &rusqlite::Connection) -> Result<(), StateError> {
    c.execute_batch("CREATE TABLE IF NOT EXISTS ef_session_ids(id TEXT PRIMARY KEY, deleted INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE IF NOT EXISTS ef_sessions(id TEXT PRIMARY KEY REFERENCES ef_session_ids(id), owner_session_id TEXT, revision INTEGER NOT NULL, status TEXT NOT NULL, record TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS ef_goals(id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES ef_sessions(id), revision INTEGER NOT NULL, status TEXT NOT NULL, record TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS ef_sessions_owner ON ef_sessions(owner_session_id);
        CREATE INDEX IF NOT EXISTS ef_goals_session ON ef_goals(session_id);
        CREATE UNIQUE INDEX IF NOT EXISTS ef_one_open_goal ON ef_goals(session_id) WHERE status IN ('ACTIVE','PAUSED','BLOCKED');
        CREATE TABLE IF NOT EXISTS ef_goal_criteria(goal_id TEXT NOT NULL REFERENCES ef_goals(id) ON DELETE CASCADE, id TEXT NOT NULL, position INTEGER NOT NULL, record TEXT NOT NULL, PRIMARY KEY(goal_id,id));
        CREATE TABLE IF NOT EXISTS ef_runs(id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES ef_sessions(id), owner_session_id TEXT NOT NULL, scope TEXT NOT NULL, status TEXT NOT NULL, lease_until INTEGER NOT NULL, record TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS ef_runs_session ON ef_runs(session_id,scope,status,lease_until);
        CREATE INDEX IF NOT EXISTS ef_runs_owner ON ef_runs(owner_session_id,status,lease_until);
        CREATE TABLE IF NOT EXISTS ef_events(id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES ef_sessions(id), run_id TEXT, goal_id TEXT, sequence INTEGER NOT NULL, kind TEXT NOT NULL, record TEXT NOT NULL, UNIQUE(session_id,sequence));
        CREATE UNIQUE INDEX IF NOT EXISTS ef_goal_run_judged ON ef_events(goal_id,run_id) WHERE kind='GOAL_JUDGED';
        CREATE TABLE IF NOT EXISTS ef_session_artifact_refs(session_id TEXT NOT NULL REFERENCES ef_sessions(id), artifact_id TEXT NOT NULL, PRIMARY KEY(session_id,artifact_id));
        CREATE TABLE IF NOT EXISTS ef_artifact_ownership(artifact_id TEXT PRIMARY KEY);
        CREATE TABLE IF NOT EXISTS ef_artifact_cleanup(artifact_id TEXT PRIMARY KEY);
        CREATE TABLE IF NOT EXISTS ef_session_context(session_id TEXT NOT NULL REFERENCES ef_sessions(id), key TEXT NOT NULL, revision INTEGER NOT NULL, value TEXT NOT NULL, PRIMARY KEY(session_id,key));") .map_err(sql)
}

fn session(c: &rusqlite::Connection, id: &str) -> Result<SessionRecord, StateError> {
    let row: Option<String> = c
        .query_row("SELECT record FROM ef_sessions WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .optional()
        .map_err(sql)?;
    decode(row.ok_or(StateError::NotFound)?)
}
pub(crate) fn require_session(
    c: &rusqlite::Connection,
    id: &str,
    active: bool,
) -> Result<SessionRecord, StateError> {
    let session = session(c, id)?;
    if active && session.status != SessionStatus::Active {
        return Err(StateError::Conflict);
    }
    Ok(session)
}
pub(crate) fn ensure_session(c: &rusqlite::Connection, id: &str) -> Result<(), StateError> {
    if id.trim().is_empty() {
        return Err(StateError::Invalid(
            "session identity cannot be blank".into(),
        ));
    }
    match session(c, id) {
        Ok(_) => {
            require_session(c, id, true)?;
            return Ok(());
        }
        Err(StateError::NotFound) => {}
        Err(error) => return Err(error),
    }
    let deleted: Option<bool> = c
        .query_row(
            "SELECT deleted FROM ef_session_ids WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql)?;
    if deleted.is_some() {
        return Err(StateError::Conflict);
    }
    let mut record = SessionRecord::new("");
    record.id = id.into();
    insert_session(c, &record)?;
    Ok(())
}
fn insert_session(c: &rusqlite::Connection, record: &SessionRecord) -> Result<(), StateError> {
    c.execute("INSERT INTO ef_session_ids(id) VALUES (?1)", [&record.id])
        .map_err(sql)?;
    c.execute(
        "INSERT INTO ef_sessions(id,revision,status,record) VALUES (?1,?2,?3,?4)",
        params![
            record.id,
            record.revision,
            status(&record.status)?,
            encode(record)?
        ],
    )
    .map_err(sql)?;
    append(
        c,
        SessionEvent::new(&record.id, "SESSION_CREATED", json!({"title":record.title})),
    )?;
    Ok(())
}
fn append(c: &rusqlite::Connection, mut event: SessionEvent) -> Result<SessionEvent, StateError> {
    require_session(c, &event.session_id, false)?;
    let existing: Option<String> = c
        .query_row(
            "SELECT record FROM ef_events WHERE id=?1",
            [&event.id],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql)?;
    if let Some(existing) = existing {
        let prior: SessionEvent = decode(existing)?;
        if prior.session_id != event.session_id
            || prior.run_id != event.run_id
            || prior.goal_id != event.goal_id
            || prior.kind != event.kind
            || prior.payload != event.payload
        {
            return Err(StateError::Conflict);
        }
        return Ok(prior);
    }
    event.sequence = c
        .query_row(
            "SELECT COALESCE(max(sequence),0)+1 FROM ef_events WHERE session_id=?1",
            [&event.session_id],
            |r| r.get(0),
        )
        .map_err(sql)?;
    c.execute("INSERT INTO ef_events(id,session_id,run_id,goal_id,sequence,kind,record) VALUES (?1,?2,?3,?4,?5,?6,?7)", params![event.id,event.session_id,event.run_id.map(|v|v.to_string()),event.goal_id,event.sequence,event.kind,encode(&event)?]).map_err(sql)?;
    Ok(event)
}
pub(crate) fn append_journal(
    c: &rusqlite::Connection,
    envelope: &EventEnvelope,
) -> Result<(), StateError> {
    let id = format!("journal/{}/{}", envelope.run_id, envelope.sequence);
    let exists: bool = c
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM ef_events WHERE id=?1)",
            [&id],
            |r| r.get(0),
        )
        .map_err(sql)?;
    if exists {
        return Ok(());
    }
    let mut event = SessionEvent::new(
        &envelope.session_id,
        "EXECUTION_JOURNAL",
        serde_json::to_value(envelope).map_err(|e| StateError::Invalid(e.to_string()))?,
    );
    event.id = id;
    event.run_id = Some(envelope.run_id);
    append(c, event)?;
    Ok(())
}
fn goal(c: &rusqlite::Connection, id: &str) -> Result<GoalRecord, StateError> {
    let row: Option<String> = c
        .query_row("SELECT record FROM ef_goals WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .optional()
        .map_err(sql)?;
    decode(row.ok_or(StateError::NotFound)?)
}
fn criteria(c: &rusqlite::Connection, goal: &GoalRecord) -> Result<(), StateError> {
    c.execute("DELETE FROM ef_goal_criteria WHERE goal_id=?1", [&goal.id])
        .map_err(sql)?;
    for (position, criterion) in goal.criteria.iter().enumerate() {
        c.execute(
            "INSERT INTO ef_goal_criteria(goal_id,id,position,record) VALUES (?1,?2,?3,?4)",
            params![goal.id, criterion.id, position, encode(criterion)?],
        )
        .map_err(sql)?;
    }
    Ok(())
}
fn bounded(limit: usize) -> Result<usize, StateError> {
    if limit == 0 || limit > 1000 {
        Err(StateError::Invalid(
            "page size must be between 1 and 1000".into(),
        ))
    } else {
        Ok(limit)
    }
}
pub(crate) fn busy(c: &rusqlite::Connection, id: &str) -> Result<bool, StateError> {
    for (table, column, expiry) in [
        ("ef_checkpoints", "session_id", "expires"),
        ("ef_conversations", "session", "lease_until"),
    ] {
        let exists: bool = c
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name=?1)",
                [table],
                |r| r.get(0),
            )
            .map_err(sql)?;
        if exists && c.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE {column}=?1 AND owner IS NOT NULL AND {expiry}>?2)"),params![id,lifecycle_time_ms()],|r|r.get::<_,bool>(0)).map_err(sql)? { return Ok(true); }
    }
    c.query_row("SELECT EXISTS(SELECT 1 FROM ef_runs WHERE (session_id=?1 OR owner_session_id=?1) AND status='RUNNING' AND lease_until>?2)", params![id,lifecycle_time_ms()], |r|r.get(0)).map_err(sql)
}

impl SqliteLifecycleStore {
    pub async fn with_database(database: SqliteDatabase) -> Result<Self, StateError> {
        database.call(|c| initialize(c)).await.map_err(worker)?;
        Ok(Self {
            database,
            closed: std::sync::Arc::default(),
        })
    }
    pub fn close(&self) {
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
    }
    async fn execute<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut rusqlite::Connection) -> Result<T, StateError> + Send + 'static,
    ) -> Result<T, tokio_rusqlite::Error<StateError>> {
        if self.closed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(tokio_rusqlite::Error::Error(StateError::Backend(
                "session lifecycle store is closed".into(),
            )));
        }
        self.database.call(operation).await
    }
    pub fn database(&self) -> SqliteDatabase {
        self.database.clone()
    }
}

#[async_trait]
impl SessionLifecycleStore for SqliteLifecycleStore {
    async fn create_session(&self, record: SessionRecord) -> Result<SessionRecord, StateError> {
        if record.id.trim().is_empty()
            || record.revision != 0
            || record.status != SessionStatus::Active
        {
            return Err(StateError::Invalid(
                "new sessions must be active at revision zero with a nonempty identity".into(),
            ));
        }
        self.execute(move |c| {
            let tx = begin(c).map_err(sql)?;
            insert_session(&tx, &record)?;
            tx.commit().map_err(sql)?;
            Ok(record)
        })
        .await
        .map_err(worker)
    }
    async fn get_session(&self, id: &str) -> Result<SessionRecord, StateError> {
        let id = id.to_owned();
        self.execute(move |c| session(c, &id)).await.map_err(worker)
    }
    async fn list_sessions(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SessionRecord>, StateError> {
        let limit = bounded(limit)?;
        let after = after.unwrap_or("").to_owned();
        self.execute(move |c| {
            let mut query = c
                .prepare("SELECT record FROM ef_sessions WHERE id>?1 ORDER BY id LIMIT ?2")
                .map_err(sql)?;
            query
                .query_map(params![after, limit], |r| r.get::<_, String>(0))
                .map_err(sql)?
                .map(|r| decode(r.map_err(sql)?))
                .collect()
        })
        .await
        .map_err(worker)
    }
    async fn update_session(
        &self,
        mut record: SessionRecord,
        expected: u64,
    ) -> Result<SessionRecord, StateError> {
        self.execute(move |c| {
            let tx = begin(c).map_err(sql)?;
            let prior = session(&tx, &record.id)?;
            if prior.revision != expected
                || record.revision != expected
                || prior.created_at_ms != record.created_at_ms
            {
                return Err(StateError::Conflict);
            }
            if record.status == SessionStatus::Archived && busy(&tx, &record.id)? {
                return Err(StateError::Busy);
            }
            record.revision = expected.checked_add(1).ok_or(StateError::Conflict)?;
            record.updated_at_ms = lifecycle_time_ms();
            tx.execute(
                "UPDATE ef_sessions SET revision=?2,status=?3,record=?4 WHERE id=?1",
                params![
                    record.id,
                    record.revision,
                    status(&record.status)?,
                    encode(&record)?
                ],
            )
            .map_err(sql)?;
            append(
                &tx,
                SessionEvent::new(
                    &record.id,
                    "SESSION_UPDATED",
                    json!({"title":record.title,"status":record.status}),
                ),
            )?;
            tx.commit().map_err(sql)?;
            Ok(record)
        })
        .await
        .map_err(worker)
    }
    async fn create_goal(&self, record: GoalRecord) -> Result<GoalRecord, StateError> {
        record.validate()?;
        if record.revision != 0 || record.runs_used != 0 || record.status != GoalStatus::Active {
            return Err(StateError::Invalid(
                "new goal must be active at revision zero with no judged runs".into(),
            ));
        }
        self.execute(move |c| { let tx=begin(c).map_err(sql)?; require_session(&tx,&record.session_id,true)?;
            tx.execute("INSERT INTO ef_goals(id,session_id,revision,status,record) VALUES (?1,?2,0,'ACTIVE',?3)",params![record.id,record.session_id,encode(&record)?]).map_err(sql)?;
            criteria(&tx,&record)?; let mut event=SessionEvent::new(&record.session_id,"GOAL_SET",json!({"objective":record.objective})); event.goal_id=Some(record.id.clone()); append(&tx,event)?;
            tx.commit().map_err(sql)?; Ok(record)
        }).await.map_err(worker)
    }
    async fn get_goal(&self, id: &str) -> Result<GoalRecord, StateError> {
        let id = id.to_owned();
        self.execute(move |c| goal(c, &id)).await.map_err(worker)
    }
    async fn active_goal(&self, id: &str) -> Result<Option<GoalRecord>, StateError> {
        let id = id.to_owned();
        self.execute(move |c| {
            let row: Option<String>=c.query_row("SELECT record FROM ef_goals WHERE session_id=?1 AND status IN ('ACTIVE','PAUSED','BLOCKED')",[id],|r|r.get(0)).optional().map_err(sql)?; row.map(decode).transpose()
        }).await.map_err(worker)
    }
    async fn list_goals(&self, session: &str) -> Result<Vec<GoalRecord>, StateError> {
        let id = session.to_owned();
        self.execute(move |c| {
            let mut q = c
                .prepare("SELECT record FROM ef_goals WHERE session_id=?1 ORDER BY rowid")
                .map_err(sql)?;
            q.query_map([id], |r| r.get::<_, String>(0))
                .map_err(sql)?
                .map(|r| decode(r.map_err(sql)?))
                .collect()
        })
        .await
        .map_err(worker)
    }
    async fn update_goal(
        &self,
        mut record: GoalRecord,
        expected: u64,
        event: SessionEvent,
    ) -> Result<GoalRecord, StateError> {
        record.validate()?;
        self.execute(move |c| {
            let tx = begin(c).map_err(sql)?;
            let prior = goal(&tx, &record.id)?;
            require_session(&tx, &record.session_id, true)?;
            if prior.revision != expected
                || record.revision != expected
                || prior.session_id != record.session_id
                || (!prior.status.is_open()
                    && !(record.status == GoalStatus::Cleared
                        && event.kind == "GOAL_STATE_CHANGED"))
                || event.session_id != record.session_id
                || event.goal_id.as_deref() != Some(&record.id)
            {
                return Err(StateError::Conflict);
            }
            if event.kind == "GOAL_JUDGED" {
                let run_id = event
                    .run_id
                    .ok_or_else(|| StateError::Invalid("goal judgement needs a run".into()))?;
                let row: Option<String> = tx
                    .query_row(
                        "SELECT record FROM ef_runs WHERE id=?1",
                        [run_id.to_string()],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(sql)?;
                let run: ManagedRun = decode(row.ok_or(StateError::NotFound)?)?;
                if run.goal_id.as_deref() != Some(&record.id)
                    || run.status != ManagedRunStatus::Completed
                {
                    return Err(StateError::Conflict);
                }
            }
            record.revision = expected.checked_add(1).ok_or(StateError::Conflict)?;
            record.updated_at_ms = lifecycle_time_ms();
            tx.execute(
                "UPDATE ef_goals SET revision=?2,status=?3,record=?4 WHERE id=?1",
                params![
                    record.id,
                    record.revision,
                    status(&record.status)?,
                    encode(&record)?
                ],
            )
            .map_err(sql)?;
            criteria(&tx, &record)?;
            append(&tx, event)?;
            tx.commit().map_err(sql)?;
            Ok(record)
        })
        .await
        .map_err(worker)
    }
    async fn register_run(&self, run: ManagedRun, resume: bool) -> Result<(), StateError> {
        self.execute(move |c| { let tx=begin(c).map_err(sql)?; ensure_session(&tx,&run.session_id)?;
            let occupied: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM ef_runs WHERE session_id=?1 AND scope=?2 AND id<>?3 AND status='RUNNING' AND lease_until>?4)",params![run.session_id,run.execution_scope,run.id.to_string(),lifecycle_time_ms()],|r|r.get(0)).map_err(sql)?;
            if occupied { return Err(StateError::Busy); }
            if run.execution_scope.trim().is_empty() || run.kind.trim().is_empty() { return Err(StateError::Invalid("run scope and kind cannot be blank".into())); }
            if run.status!=ManagedRunStatus::Running { return Err(StateError::Invalid("admitted run must be running".into())); }
            let mut owner=run.session_id.clone();
            if let Some(parent)=run.parent_run_id {
                owner=tx.query_row("SELECT owner_session_id FROM ef_runs WHERE id=?1",[parent.to_string()],|r|r.get(0)).map_err(sql)?;
                require_session(&tx,&owner,true)?;
                if owner!=run.session_id { tx.execute("UPDATE ef_sessions SET owner_session_id=?2 WHERE id=?1",params![run.session_id,owner]).map_err(sql)?; }
            }
            if let Some(id)=&run.goal_id && goal(&tx,id)?.session_id!=owner { return Err(StateError::Conflict); }
            if resume {
                let old: String=tx.query_row("SELECT record FROM ef_runs WHERE id=?1",[run.id.to_string()],|r|r.get(0)).map_err(sql)?;
                let old: ManagedRun=decode(old)?;
                if old.session_id!=run.session_id || old.execution_scope!=run.execution_scope || (old.status==ManagedRunStatus::Running && old.lease_until_ms>lifecycle_time_ms()) { return Err(StateError::Busy); }
                tx.execute("UPDATE ef_runs SET status='RUNNING',lease_until=?2,record=?3 WHERE id=?1",params![run.id.to_string(),run.lease_until_ms,encode(&run)?]).map_err(sql)?;
            } else {
                tx.execute("INSERT INTO ef_runs(id,session_id,owner_session_id,scope,status,lease_until,record) VALUES (?1,?2,?3,?4,'RUNNING',?5,?6)",params![run.id.to_string(),run.session_id,owner,run.execution_scope,run.lease_until_ms,encode(&run)?]).map_err(sql)?;
            }
            let mut event=SessionEvent::new(&run.session_id,if resume {"RUN_RESUMED"} else {"RUN_STARTED"},json!({"scope":run.execution_scope,"kind":run.kind,"parent":run.parent_run_id})); event.run_id=Some(run.id); event.goal_id=run.goal_id.clone(); append(&tx,event)?;
            tx.commit().map_err(sql)
        }).await.map_err(worker)
    }
    async fn finish_run(&self, run: ManagedRun) -> Result<(), StateError> {
        self.execute(move |c| { let tx=begin(c).map_err(sql)?;
            let changed=tx.execute("UPDATE ef_runs SET status=?2,lease_until=0,record=?3 WHERE id=?1 AND status='RUNNING'",params![run.id.to_string(),status(&run.status)?,encode(&run)?]).map_err(sql)?;
            if changed!=1 { return Err(StateError::Conflict); }
            if !matches!(run.status,ManagedRunStatus::Completed) && let Some(id)=&run.goal_id {
                    let mut g=goal(&tx,id)?;
                    if g.status==GoalStatus::Active {
                        g.status=GoalStatus::Paused; g.reason=Some("execution_requires_attention".into()); g.revision+=1; g.updated_at_ms=lifecycle_time_ms();
                        tx.execute("UPDATE ef_goals SET status='PAUSED',revision=?2,record=?3 WHERE id=?1",params![g.id,g.revision,encode(&g)?]).map_err(sql)?;
                        let mut e=SessionEvent::new(&g.session_id,"GOAL_STATE_CHANGED",json!({"status":"PAUSED","reason":g.reason})); e.goal_id=Some(g.id); append(&tx,e)?;
                    }
            }
            let mut event=SessionEvent::new(&run.session_id,"RUN_FINISHED",json!({"status":run.status,"error":run.error})); event.run_id=Some(run.id); event.goal_id=run.goal_id.clone(); append(&tx,event)?;
            tx.commit().map_err(sql)
        }).await.map_err(worker)
    }
    async fn get_run(&self, id: RunId) -> Result<ManagedRun, StateError> {
        self.execute(move |c| {
            let row: Option<String> = c
                .query_row(
                    "SELECT record FROM ef_runs WHERE id=?1",
                    [id.to_string()],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql)?;
            decode(row.ok_or(StateError::NotFound)?)
        })
        .await
        .map_err(worker)
    }
    async fn list_runs(&self, session: &str) -> Result<Vec<ManagedRun>, StateError> {
        let id = session.to_owned();
        self.execute(move |c| { let mut q=c.prepare("SELECT record FROM ef_runs WHERE session_id=?1 OR owner_session_id=?1 ORDER BY rowid").map_err(sql)?;
            q.query_map([id],|r|r.get::<_,String>(0)).map_err(sql)?.map(|r|decode(r.map_err(sql)?)).collect()
        }).await.map_err(worker)
    }
    async fn append_event(&self, event: SessionEvent) -> Result<SessionEvent, StateError> {
        self.execute(move |c| {
            let tx = begin(c).map_err(sql)?;
            let event = append(&tx, event)?;
            tx.commit().map_err(sql)?;
            Ok(event)
        })
        .await
        .map_err(worker)
    }
    async fn list_events(
        &self,
        session: &str,
        after: u64,
        limit: usize,
    ) -> Result<Vec<SessionEvent>, StateError> {
        let limit = bounded(limit)?;
        let id = session.to_owned();
        self.execute(move |c| { let mut q=c.prepare("SELECT record FROM ef_events WHERE session_id=?1 AND sequence>?2 ORDER BY sequence LIMIT ?3").map_err(sql)?;
            q.query_map(params![id,after,limit],|r|r.get::<_,String>(0)).map_err(sql)?.map(|r|decode(r.map_err(sql)?)).collect()
        }).await.map_err(worker)
    }
    async fn recover_expired_runs(&self) -> Result<usize, StateError> {
        self.execute(|c| { let tx=begin(c).map_err(sql)?;
            let rows: Vec<String>={ let mut q=tx.prepare("SELECT record FROM ef_runs WHERE status='RUNNING' AND lease_until<=?1").map_err(sql)?;
                q.query_map([lifecycle_time_ms()],|r|r.get(0)).map_err(sql)?.collect::<Result<_,_>>().map_err(sql)? };
            for row in &rows {
                let mut run: ManagedRun=decode(row.clone())?; run.status=ManagedRunStatus::RecoveryRequired;
                tx.execute("UPDATE ef_runs SET status='RECOVERY_REQUIRED',record=?2 WHERE id=?1",params![run.id.to_string(),encode(&run)?]).map_err(sql)?;
                let mut event=SessionEvent::new(&run.session_id,"RECOVERY_REQUIRED",Value::Null); event.run_id=Some(run.id); append(&tx,event)?;
                let active: Option<String>=tx.query_row("SELECT id FROM ef_goals WHERE session_id=(SELECT owner_session_id FROM ef_runs WHERE id=?1) AND status='ACTIVE'",[run.id.to_string()],|r|r.get(0)).optional().map_err(sql)?;
                if let Some(id)=active { let mut g=goal(&tx,&id)?; g.status=GoalStatus::Paused; g.reason=Some("recovery_required".into()); g.revision+=1; g.updated_at_ms=lifecycle_time_ms();
                    tx.execute("UPDATE ef_goals SET status='PAUSED',revision=?2,record=?3 WHERE id=?1",params![id,g.revision,encode(&g)?]).map_err(sql)?;
                    let mut e=SessionEvent::new(&g.session_id,"GOAL_RECOVERY_REQUIRED",Value::Null); e.goal_id=Some(id); append(&tx,e)?;
                }
            }
            tx.commit().map_err(sql)?; Ok(rows.len())
        }).await.map_err(worker)
    }
    async fn read_context(
        &self,
        session: &str,
        key: &str,
    ) -> Result<Option<(u64, Value)>, StateError> {
        let session = session.to_owned();
        let key = key.to_owned();
        self.execute(move |c| {
            require_session(c, &session, false)?;
            let row: Option<(u64, String)> = c
                .query_row(
                    "SELECT revision,value FROM ef_session_context WHERE session_id=?1 AND key=?2",
                    params![session, key],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(sql)?;
            row.map(|(revision, value)| Ok((revision, decode(value)?)))
                .transpose()
        })
        .await
        .map_err(worker)
    }
    async fn write_context(
        &self,
        session: &str,
        key: &str,
        expected: Option<u64>,
        value: Value,
    ) -> Result<u64, StateError> {
        let session = session.to_owned();
        let key = key.to_owned();
        if key.trim().is_empty() {
            return Err(StateError::Invalid("context key cannot be blank".into()));
        }
        self.execute(move |c| {
            let tx=begin(c).map_err(sql)?; require_session(&tx,&session,true)?;
            let prior: Option<u64>=tx.query_row("SELECT revision FROM ef_session_context WHERE session_id=?1 AND key=?2",params![session,key],|r|r.get(0)).optional().map_err(sql)?;
            if prior!=expected { return Err(StateError::Conflict); }
            let revision=prior.map_or(Some(0),|v|v.checked_add(1)).ok_or(StateError::Conflict)?;
            tx.execute("INSERT INTO ef_session_context(session_id,key,revision,value) VALUES (?1,?2,?3,?4) ON CONFLICT(session_id,key) DO UPDATE SET revision=excluded.revision,value=excluded.value",params![session,key,revision,encode(&value)?]).map_err(sql)?;
            append(&tx,SessionEvent::new(session,"CONTEXT_UPDATED",json!({"key":key,"revision":revision})))?;
            tx.commit().map_err(sql)?; Ok(revision)
        }).await.map_err(worker)
    }
    async fn attach_artifact(
        &self,
        session: &str,
        artifact: &str,
        owned: bool,
    ) -> Result<(), StateError> {
        let session = session.to_owned();
        let artifact = artifact.to_owned();
        if crate::ArtifactId::parse(artifact.clone()).is_err() {
            return Err(StateError::Invalid("artifact ID cannot be blank".into()));
        }
        self.execute(move |c| {
            let tx=begin(c).map_err(sql)?; require_session(&tx,&session,true)?;
            let exists: bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM ef_local_artifact_metadata WHERE id=?1)",[&artifact],|r|r.get(0)).map_err(sql)?;
            if !exists {return Err(StateError::NotFound);}
            tx.execute("INSERT OR IGNORE INTO ef_session_artifact_refs(session_id,artifact_id) VALUES (?1,?2)",params![session,artifact]).map_err(sql)?;
            if owned { tx.execute("INSERT OR IGNORE INTO ef_artifact_ownership(artifact_id) VALUES (?1)",[&artifact]).map_err(sql)?; }
            tx.execute("DELETE FROM ef_artifact_cleanup WHERE artifact_id=?1",[&artifact]).map_err(sql)?;
            append(&tx,SessionEvent::new(session,"ARTIFACT_ATTACHED",json!({"artifact_id":artifact,"owned":owned})))?;
            tx.commit().map_err(sql)
        }).await.map_err(worker)
    }
}

/// Database visibility is authoritative; failed file cleanup remains retryable.
pub(crate) async fn cleanup_artifacts(database: &SqliteDatabase, files: &crate::FileArtifactStore) {
    use crate::{ArtifactError, ArtifactId, ArtifactStore};
    let result=database.transaction(async {
        let ids=database.call(|c| {
            let mut query=c.prepare("SELECT artifact_id FROM ef_artifact_cleanup").map_err(sql)?;
            query.query_map([],|r|r.get::<_,String>(0)).map_err(sql)?.collect::<Result<Vec<_>,_>>().map_err(sql)
        }).await.map_err(worker)?;
        for id in ids {
            let artifact=ArtifactId::parse(id.clone()).map_err(|e|StateError::Invalid(e.to_string()))?;
            match files.remove(&artifact,tokio_util::sync::CancellationToken::new()).await {
                Ok(()) | Err(ArtifactError::NotFound) => {
                    database.call(move |c| {
                        c.execute("DELETE FROM ef_artifact_cleanup WHERE artifact_id=?1",[&id]).map_err(sql)?;
                        c.execute("DELETE FROM ef_artifact_ownership WHERE artifact_id=?1",[&id]).map_err(sql)?;
                        Ok(())
                    }).await.map_err(worker)?;
                }
                Err(error) => tracing::warn!(artifact_id=%id,%error,"session artifact cleanup will be retried"),
            }
        }
        Ok(())
    }).await;
    if let Err(error) = result {
        tracing::warn!(%error,"session artifact cleanup will be retried");
    }
}
