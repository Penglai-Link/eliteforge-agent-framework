use crate::{SqliteStateStore, lifecycle::sql};
use async_trait::async_trait;
use eliteforge_agent_core::*;
use rusqlite::{OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};

fn encode(value: &impl Serialize) -> Result<String, StateError> {
    serde_json::to_string(value).map_err(|e| StateError::Invalid(e.to_string()))
}
fn decode<T: DeserializeOwned>(value: String) -> Result<T, StateError> {
    serde_json::from_str(&value).map_err(|_| StateError::Invalid("corrupt history record".into()))
}
pub(crate) fn initialize(c: &rusqlite::Connection) -> Result<(), StateError> {
    c.execute_batch("CREATE TABLE IF NOT EXISTS ef_history_versions(session_id TEXT PRIMARY KEY, version TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS ef_history_runs(ordinal INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE, session_id TEXT NOT NULL, owner_session_id TEXT NOT NULL, root_run_id TEXT NOT NULL, active INTEGER NOT NULL, record TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS ef_history_turns ON ef_history_runs(owner_session_id, ordinal DESC) WHERE id=root_run_id;
        CREATE INDEX IF NOT EXISTS ef_history_roots ON ef_history_runs(owner_session_id, root_run_id, ordinal);
        CREATE INDEX IF NOT EXISTS ef_history_active ON ef_history_runs(owner_session_id, active);
        CREATE TABLE IF NOT EXISTS ef_history_steps(id TEXT PRIMARY KEY, run_id TEXT NOT NULL, session_id TEXT NOT NULL, position INTEGER NOT NULL, record TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS ef_history_step_order ON ef_history_steps(run_id, position);
        CREATE INDEX IF NOT EXISTS ef_history_step_session ON ef_history_steps(session_id);
        CREATE TABLE IF NOT EXISTS ef_history_messages(id TEXT PRIMARY KEY, run_id TEXT NOT NULL, session_id TEXT NOT NULL, position INTEGER NOT NULL, record TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS ef_history_message_order ON ef_history_messages(run_id, position);
        CREATE INDEX IF NOT EXISTS ef_history_message_session ON ef_history_messages(session_id);
        CREATE TABLE IF NOT EXISTS ef_history_tools(id TEXT PRIMARY KEY, run_id TEXT NOT NULL, session_id TEXT NOT NULL, position INTEGER NOT NULL, record TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS ef_history_tool_order ON ef_history_tools(run_id, position);
        CREATE INDEX IF NOT EXISTS ef_history_tool_session ON ef_history_tools(session_id);
        CREATE TABLE IF NOT EXISTS ef_history_interactions(id TEXT PRIMARY KEY, run_id TEXT NOT NULL, session_id TEXT NOT NULL, position INTEGER NOT NULL, answered INTEGER NOT NULL, record TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS ef_history_interaction_order ON ef_history_interactions(run_id, position);
        CREATE INDEX IF NOT EXISTS ef_history_interaction_pending ON ef_history_interactions(session_id, answered);") .map_err(sql)
}
pub(crate) fn touch(c: &rusqlite::Connection, owner: &str) -> Result<(), StateError> {
    c.execute("INSERT INTO ef_history_versions VALUES (?1,?2) ON CONFLICT(session_id) DO UPDATE SET version=excluded.version", params![owner, RunId::new_v4().to_string()]).map_err(sql)?;
    Ok(())
}
fn owner(c: &rusqlite::Connection, run_id: RunId) -> Result<String, StateError> {
    c.query_row(
        "SELECT owner_session_id FROM ef_history_runs WHERE id=?1",
        [run_id.to_string()],
        |r| r.get(0),
    )
    .map_err(sql)
}
fn put_run(c: &rusqlite::Connection, run: &HistoryRun, owner: &str) -> Result<(), StateError> {
    let active = matches!(
        run.status,
        HistoryStatus::Running | HistoryStatus::Waiting | HistoryStatus::Unknown
    );
    c.execute("INSERT INTO ef_history_runs(id,session_id,owner_session_id,root_run_id,active,record) VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(id) DO UPDATE SET owner_session_id=excluded.owner_session_id,root_run_id=excluded.root_run_id,active=excluded.active,record=excluded.record",
        params![run.id.to_string(),run.session_id,owner,run.root_run_id.to_string(),active,encode(run)?]).map_err(sql)?;
    touch(c, owner)
}
pub(crate) fn managed_run(
    c: &rusqlite::Connection,
    run: &ManagedRun,
    owner: &str,
) -> Result<(), StateError> {
    if matches!(
        run.status,
        ManagedRunStatus::Cancelled | ManagedRunStatus::Failed | ManagedRunStatus::RecoveryRequired
    ) {
        interrupt_unfinished(c, run.id)?;
    }
    put_run(
        c,
        &HistoryRun {
            id: run.id,
            session_id: run.session_id.clone(),
            parent_run_id: run.parent_run_id,
            root_run_id: run.root_run_id,
            status: match run.status {
                ManagedRunStatus::Running => HistoryStatus::Running,
                ManagedRunStatus::AwaitingInput => HistoryStatus::Waiting,
                ManagedRunStatus::Cancelled => HistoryStatus::Interrupted,
                ManagedRunStatus::Failed => HistoryStatus::Failed,
                ManagedRunStatus::RecoveryRequired => HistoryStatus::Unknown,
                _ => HistoryStatus::Completed,
            },
            started_at_ms: run.started_at_ms,
            finished_at_ms: run.finished_at_ms,
            error: run.error.clone(),
        },
        owner,
    )
}
fn interrupt_unfinished(c: &rusqlite::Connection, run: RunId) -> Result<(), StateError> {
    let now = unix_time_ms();
    for mut record in records::<HistoryMessage>(c, "ef_history_messages", run)? {
        if record.status == HistoryStatus::Running {
            record.status = HistoryStatus::Interrupted;
            c.execute(
                "UPDATE ef_history_messages SET record=?2 WHERE id=?1",
                params![record.id, encode(&record)?],
            )
            .map_err(sql)?;
        }
    }
    for mut record in records::<HistoryStep>(c, "ef_history_steps", run)? {
        if record.status == HistoryStatus::Running {
            record.status = HistoryStatus::Interrupted;
            record.finished_at_ms = Some(now);
            c.execute(
                "UPDATE ef_history_steps SET record=?2 WHERE id=?1",
                params![record.id, encode(&record)?],
            )
            .map_err(sql)?;
        }
    }
    for mut record in records::<HistoryToolInvocation>(c, "ef_history_tools", run)? {
        if record.status == HistoryStatus::Running {
            record.status = HistoryStatus::Unknown;
            c.execute(
                "UPDATE ef_history_tools SET record=?2 WHERE id=?1",
                params![record.id.to_string(), encode(&record)?],
            )
            .map_err(sql)?;
        }
    }
    Ok(())
}
fn put<T: Serialize>(
    c: &rusqlite::Connection,
    table: &str,
    id: &str,
    checkpoint: &Checkpoint,
    position: usize,
    record: &T,
) -> Result<(), StateError> {
    let changed=c.execute(&format!("INSERT INTO {table}(id,run_id,session_id,position,record) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET record=excluded.record WHERE {table}.run_id=excluded.run_id AND {table}.session_id=excluded.session_id"),
        params![id,checkpoint.run_id.to_string(),checkpoint.session_id,position,encode(record)?]).map_err(sql)?;
    if changed != 1 {
        return Err(StateError::Conflict);
    }
    Ok(())
}
/// Invoked inside the checkpoint transaction, after fencing/revision validation.
pub(crate) fn checkpoint(
    c: &rusqlite::Connection,
    next: &Checkpoint,
    previous: Option<&Checkpoint>,
) -> Result<(), StateError> {
    let old: Option<String> = c
        .query_row(
            "SELECT record FROM ef_history_runs WHERE id=?1",
            [next.run_id.to_string()],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql)?;
    let mut run: HistoryRun = old.map(decode).transpose()?.unwrap_or(HistoryRun {
        id: next.run_id,
        session_id: next.session_id.clone(),
        parent_run_id: None,
        root_run_id: next.run_id,
        status: HistoryStatus::Running,
        started_at_ms: unix_time_ms(),
        finished_at_ms: None,
        error: None,
    });
    run.status = match next.status {
        ExecutionStatus::Running => HistoryStatus::Running,
        ExecutionStatus::Waiting => HistoryStatus::Waiting,
        ExecutionStatus::Completed => HistoryStatus::Completed,
        ExecutionStatus::Cancelled => HistoryStatus::Interrupted,
        ExecutionStatus::Failed => HistoryStatus::Failed,
    };
    if next.status.is_terminal() {
        run.finished_at_ms = Some(unix_time_ms());
    }
    let owner = c
        .query_row(
            "SELECT owner_session_id FROM ef_history_runs WHERE id=?1",
            [next.run_id.to_string()],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .map_err(sql)?
        .unwrap_or(next.session_id.clone());
    put_run(c, &run, &owner)?;
    let empty = HistoryRecords::default();
    let old = previous.map(|p| &p.history).unwrap_or(&empty);
    for (i, record) in next.history.steps.iter().enumerate() {
        if !old.steps.contains(record) {
            put(c, "ef_history_steps", &record.id, next, i, record)?;
        }
    }
    for (i, record) in next.history.messages.iter().enumerate() {
        if !old.messages.contains(record) {
            put(c, "ef_history_messages", &record.id, next, i, record)?;
        }
    }
    for (i, record) in next.history.tool_invocations.iter().enumerate() {
        if !old.tool_invocations.contains(record) {
            put(
                c,
                "ef_history_tools",
                &record.id.to_string(),
                next,
                i,
                record,
            )?;
        }
    }
    for (i, record) in next.history.interactions.iter().enumerate() {
        // Answers are owned by InteractionStore; a later checkpoint must not
        // replace its already committed answer with the original pending row.
        let existing: Option<String> = c
            .query_row(
                "SELECT run_id FROM ef_history_interactions WHERE id=?1",
                [record.id.to_string()],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql)?;
        if existing.is_some_and(|run| run != next.run_id.to_string()) {
            return Err(StateError::Conflict);
        }
        c.execute("INSERT INTO ef_history_interactions(id,run_id,session_id,position,answered,record) VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(id) DO UPDATE SET answered=excluded.answered,record=excluded.record WHERE ef_history_interactions.answered=0",
            params![record.id.to_string(),next.run_id.to_string(),next.session_id,i,record.response.is_some(),encode(record)?]).map_err(sql)?;
    }
    Ok(())
}
pub(crate) fn answer(
    c: &rusqlite::Connection,
    record: &InteractionRecord,
) -> Result<(), StateError> {
    let old: Option<String> = c
        .query_row(
            "SELECT record FROM ef_history_interactions WHERE id=?1",
            [record.request.id.to_string()],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql)?;
    if let Some(old) = old {
        let mut interaction: HistoryInteraction = decode(old)?;
        interaction.response = record.response.as_ref().map(|r| r.value.clone());
        c.execute(
            "UPDATE ef_history_interactions SET answered=1,record=?2 WHERE id=?1",
            params![interaction.id.to_string(), encode(&interaction)?],
        )
        .map_err(sql)?;
        touch(c, &owner(c, record.run_id)?)?;
    }
    Ok(())
}
fn records<T: DeserializeOwned>(
    c: &rusqlite::Connection,
    table: &str,
    run: RunId,
) -> Result<Vec<T>, StateError> {
    let mut q = c
        .prepare(&format!(
            "SELECT record FROM {table} WHERE run_id=?1 ORDER BY position"
        ))
        .map_err(sql)?;
    q.query_map([run.to_string()], |r| r.get(0))
        .map_err(sql)?
        .map(|r| decode(r.map_err(sql)?))
        .collect()
}
#[async_trait]
impl HistoryStore for SqliteStateStore {
    async fn read_session(
        &self,
        session_id: &str,
        query: HistoryQuery,
    ) -> Result<SessionHistory, StateError> {
        if session_id.trim().is_empty()
            || !(1..=100).contains(&query.limit)
            || query
                .cursor
                .as_ref()
                .is_some_and(|c| c.before > i64::MAX as u64)
        {
            return Err(StateError::Invalid(
                "history requires a session and limit 1..100".into(),
            ));
        }
        let session_id = session_id.to_owned();
        self.execute(move |c| {
            // A deferred SQLite read transaction fixes one WAL snapshot across
            // version, run, message, tool and pending-interaction reads.
            let tx = if c.is_autocommit() {
                crate::database::RequiredTransaction::Owned(
                    c.transaction_with_behavior(rusqlite::TransactionBehavior::Deferred).map_err(sql)?
                )
            } else {
                crate::database::RequiredTransaction::Joined(c)
            };
            crate::lifecycle::require_session(&tx, &session_id, false)?;
            let version: String = tx.query_row(
                "SELECT version FROM ef_history_versions WHERE session_id=?1",
                [&session_id], |r| r.get(0)
            ).optional().map_err(sql)?.unwrap_or_else(|| "0".into());
            if let Some(cursor) = &query.cursor
                && (cursor.session_id != session_id || cursor.version != version)
            {
                return Err(StateError::Conflict);
            }
            let before = query.cursor.as_ref().map(|c| c.before).unwrap_or(i64::MAX as u64);
            let mut roots: Vec<(u64, String)> = {
                let mut q = tx.prepare(
                    "SELECT ordinal,id FROM ef_history_runs WHERE owner_session_id=?1 AND id=root_run_id AND ordinal<?2 ORDER BY ordinal DESC LIMIT ?3"
                ).map_err(sql)?;
                q.query_map(params![session_id, before, query.limit + 1], |r| Ok((r.get(0)?, r.get(1)?)))
                    .map_err(sql)?.collect::<Result<_, _>>().map_err(sql)?
            };
            let more = roots.len() > query.limit;
            roots.truncate(query.limit);
            let next_cursor = if more {
                roots.last().map(|(ordinal, _)| HistoryCursor {
                    session_id: session_id.clone(), version: version.clone(), before: *ordinal,
                })
            } else { None };
            roots.reverse();
            let mut runs = Vec::new();
            let mut history = HistoryRecords::default();
            for (_, root) in roots {
                let rows: Vec<String> = {
                    let mut q = tx.prepare(
                        "SELECT record FROM ef_history_runs WHERE owner_session_id=?1 AND root_run_id=?2 ORDER BY ordinal"
                    ).map_err(sql)?;
                    q.query_map(params![session_id, root], |r| r.get(0))
                        .map_err(sql)?.collect::<Result<_, _>>().map_err(sql)?
                };
                for row in rows {
                    let run: HistoryRun = decode(row)?;
                    history.steps.extend(records::<HistoryStep>(&tx, "ef_history_steps", run.id)?);
                    history.messages.extend(records::<HistoryMessage>(&tx, "ef_history_messages", run.id)?);
                    history.tool_invocations.extend(records::<HistoryToolInvocation>(&tx, "ef_history_tools", run.id)?);
                    history.interactions.extend(records::<HistoryInteraction>(&tx, "ef_history_interactions", run.id)?);
                    runs.push(run);
                }
            }
            let pending_interactions = {
                let mut q = tx.prepare(
                    "SELECT i.record FROM ef_history_interactions i JOIN ef_history_runs r ON r.id=i.run_id WHERE r.owner_session_id=?1 AND r.active=1 AND i.answered=0 ORDER BY r.ordinal,i.position"
                ).map_err(sql)?;
                q.query_map([&session_id], |r| r.get(0)).map_err(sql)?
                    .map(|r| decode(r.map_err(sql)?)).collect::<Result<Vec<HistoryInteraction>, _>>()?
            };
            let active_runs = {
                let mut q = tx.prepare(
                    "SELECT record FROM ef_history_runs WHERE owner_session_id=?1 AND active=1 ORDER BY ordinal"
                ).map_err(sql)?;
                q.query_map([&session_id], |r| r.get(0)).map_err(sql)?
                    .map(|r| decode(r.map_err(sql)?)).collect::<Result<Vec<HistoryRun>, _>>()?
            };
            tx.commit().map_err(sql)?;
            Ok(SessionHistory {
                session_id, version, next_cursor, runs, records: history, pending_interactions, active_runs,
            })
        }).await
    }
}
