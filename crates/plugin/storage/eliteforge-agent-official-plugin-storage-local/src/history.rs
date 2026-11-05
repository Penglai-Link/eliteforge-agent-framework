use crate::{SqliteStateStore, lifecycle::sql};
use async_trait::async_trait;
use eliteforge_agent_core::*;
use rusqlite::{OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};

#[path = "history_deletion.rs"]
mod deletion;

fn encode(value: &impl Serialize) -> Result<String, StateError> {
    serde_json::to_string(value).map_err(|e| StateError::Invalid(e.to_string()))
}
fn decode<T: DeserializeOwned>(value: String) -> Result<T, StateError> {
    serde_json::from_str(&value).map_err(|_| StateError::Invalid("corrupt history record".into()))
}
pub(crate) fn initialize(c: &rusqlite::Connection) -> Result<(), StateError> {
    c.execute_batch("CREATE TABLE IF NOT EXISTS ef_history_deleted_turns(session_id TEXT NOT NULL, root_run_id TEXT NOT NULL, revision TEXT NOT NULL, PRIMARY KEY(session_id,root_run_id));
        CREATE TABLE IF NOT EXISTS ef_history_transcript_spans(run_id TEXT PRIMARY KEY, session_id TEXT NOT NULL, branch TEXT NOT NULL, generation TEXT NOT NULL, start INTEGER NOT NULL, end INTEGER NOT NULL);
        CREATE INDEX IF NOT EXISTS ef_history_span_session ON ef_history_transcript_spans(session_id,branch,start);
        CREATE TABLE IF NOT EXISTS ef_history_versions(session_id TEXT PRIMARY KEY, version TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS ef_history_runs(ordinal INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE, session_id TEXT NOT NULL, owner_session_id TEXT NOT NULL, root_run_id TEXT NOT NULL, active INTEGER NOT NULL, record TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS ef_history_turns ON ef_history_runs(owner_session_id, ordinal DESC) WHERE id=root_run_id;
        CREATE INDEX IF NOT EXISTS ef_history_roots ON ef_history_runs(owner_session_id, root_run_id, ordinal);
        CREATE INDEX IF NOT EXISTS ef_history_active ON ef_history_runs(owner_session_id, active);
        CREATE INDEX IF NOT EXISTS ef_history_session_order ON ef_history_runs(session_id, ordinal DESC);
        CREATE INDEX IF NOT EXISTS ef_history_session_roots ON ef_history_runs(session_id, root_run_id, ordinal);
        CREATE INDEX IF NOT EXISTS ef_history_session_active ON ef_history_runs(session_id, active, ordinal);
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
fn session(c: &rusqlite::Connection, run_id: RunId) -> Result<String, StateError> {
    c.query_row(
        "SELECT session_id FROM ef_history_runs WHERE id=?1",
        [run_id.to_string()],
        |r| r.get(0),
    )
    .map_err(sql)
}
fn put_run(c: &rusqlite::Connection, run: &HistoryRun, owner: &str) -> Result<(), StateError> {
    let active = matches!(run.status, HistoryStatus::Running | HistoryStatus::Waiting);
    c.execute("INSERT INTO ef_history_runs(id,session_id,owner_session_id,root_run_id,active,record) VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(id) DO UPDATE SET owner_session_id=excluded.owner_session_id,root_run_id=excluded.root_run_id,active=excluded.active,record=excluded.record",
        params![run.id.to_string(),run.session_id,owner,run.root_run_id.to_string(),active,encode(run)?]).map_err(sql)?;
    touch(c, &run.session_id)
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
fn annotation_sources(
    c: &rusqlite::Connection,
    session: &str,
    annotations: &[MessageAnnotation],
) -> Result<(), StateError> {
    validate_annotations(annotations).map_err(StateError::Invalid)?;
    let ids = annotations
        .iter()
        .map(|a| &a.message_id)
        .collect::<Vec<_>>();
    annotation_source_ids(c, session, &ids)
}
fn annotation_source_ids(
    c: &rusqlite::Connection,
    session: &str,
    ids: &[&String],
) -> Result<(), StateError> {
    if ids.is_empty() {
        return Ok(());
    }
    let invalid: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM json_each(?1) a WHERE NOT EXISTS(SELECT 1 FROM ef_history_messages m WHERE m.id=a.value AND m.session_id=?2 AND NOT EXISTS(SELECT 1 FROM ef_history_runs r JOIN ef_history_deleted_turns d ON d.session_id=r.session_id AND d.root_run_id=r.root_run_id WHERE r.id=m.run_id)))",
        params![encode(&ids)?, session], |row| row.get(0),
    ).map_err(sql)?;
    if invalid {
        return Err(StateError::Invalid(
            "annotation source is not in this session history".into(),
        ));
    }
    Ok(())
}

/// Invoked inside the checkpoint transaction, after fencing/revision validation.
pub(crate) fn checkpoint(
    c: &rusqlite::Connection,
    next: &Checkpoint,
    previous: Option<&Checkpoint>,
) -> Result<(), StateError> {
    deletion::record_span(c, next)?;
    let mut annotation_ids = Vec::new();
    for message in &next.history.messages {
        if previous.is_some_and(|old| {
            old.history
                .messages
                .iter()
                .any(|record| record.id == message.id && record.annotations == message.annotations)
        }) {
            continue;
        }
        validate_annotations(&message.annotations).map_err(StateError::Invalid)?;
        annotation_ids.extend(message.annotations.iter().map(|a| &a.message_id));
    }
    annotation_source_ids(c, &next.session_id, &annotation_ids)?;
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
        let changed = c.execute("INSERT INTO ef_history_interactions(id,run_id,session_id,position,answered,record) VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(id) DO UPDATE SET answered=CASE WHEN ef_history_interactions.answered=0 THEN excluded.answered ELSE ef_history_interactions.answered END,record=CASE WHEN ef_history_interactions.answered=0 THEN excluded.record ELSE ef_history_interactions.record END WHERE ef_history_interactions.run_id=excluded.run_id AND ef_history_interactions.session_id=excluded.session_id",
            params![record.id.to_string(),next.run_id.to_string(),next.session_id,i,record.response.is_some(),encode(record)?]).map_err(sql)?;
        if changed != 1 {
            return Err(StateError::Conflict);
        }
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
        interaction.response_provenance = record.response_provenance.clone();
        c.execute(
            "UPDATE ef_history_interactions SET answered=1,record=?2 WHERE id=?1",
            params![interaction.id.to_string(), encode(&interaction)?],
        )
        .map_err(sql)?;
        touch(c, &session(c, record.run_id)?)?;
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
fn page_records<T: DeserializeOwned>(
    c: &rusqlite::Connection,
    table: &str,
    session: &str,
    roots: &str,
) -> Result<Vec<T>, StateError> {
    let mut q = c.prepare(&format!(
        "SELECT item.record FROM json_each(?2) page JOIN ef_history_runs r ON r.root_run_id=page.value AND r.session_id=?1 JOIN {table} item ON item.run_id=r.id ORDER BY CAST(page.key AS INTEGER),r.ordinal,item.position"
    )).map_err(sql)?;
    q.query_map(params![session, roots], |row| row.get(0))
        .map_err(sql)?
        .map(|row| decode(row.map_err(sql)?))
        .collect()
}
#[async_trait]
impl HistoryStore for SqliteStateStore {
    async fn deleted_runs(&self, session: &str) -> Result<Vec<RunId>, StateError> {
        let session = session.to_owned();
        self.execute_read(move |c| {
            let mut q = c.prepare("SELECT r.id FROM ef_history_runs r JOIN ef_history_deleted_turns d ON d.session_id=r.session_id AND d.root_run_id=r.root_run_id WHERE r.session_id=?1").map_err(sql)?;
            q.query_map([session], |r| r.get::<_, String>(0)).map_err(sql)?
                .map(|r| RunId::parse_str(&r.map_err(sql)?).map_err(|e| StateError::Invalid(e.to_string()))).collect()
        }).await
    }

    async fn delete_turn(&self, session: &str, root: RunId, expected: &str) -> Result<String, StateError> {
        let session = session.to_owned();
        let expected = expected.to_owned();
        self.execute(move |c| deletion::delete(c, &session, root, &expected)).await
    }
    async fn context_revision(&self, session: &str) -> Result<String, StateError> {
        let session = session.to_owned();
        self.execute_read(move |c| deletion::revision(c, &session)).await
    }

    async fn validate_annotation_sources(
        &self,
        session: &str,
        annotations: &[MessageAnnotation],
    ) -> Result<(), StateError> {
        let session = session.to_owned();
        let annotations = annotations.to_vec();
        self.execute_read(move |connection| annotation_sources(connection, &session, &annotations))
            .await
    }

    async fn run(&self, run_id: RunId) -> Result<HistoryRun, StateError> {
        self.execute_read(move |c| {
            let record = c
                .query_row(
                    "SELECT record FROM ef_history_runs WHERE id=?1",
                    [run_id.to_string()],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql)?
                .ok_or(StateError::NotFound)?;
            decode(record)
        })
        .await
    }
    async fn runs(&self, run_ids: &[RunId]) -> Result<Vec<HistoryRun>, StateError> {
        if run_ids.is_empty() {
            return Ok(Vec::new());
        }
        let ids = encode(&run_ids.iter().map(ToString::to_string).collect::<Vec<_>>())?;
        self.execute_read(move |c| {
            let mut q = c.prepare(
                "SELECT r.record FROM json_each(?1) requested LEFT JOIN ef_history_runs r ON r.id=requested.value ORDER BY CAST(requested.key AS INTEGER)"
            ).map_err(sql)?;
            q.query_map([ids], |row| row.get::<_, Option<String>>(0)).map_err(sql)?
                .map(|row| decode(row.map_err(sql)?.ok_or(StateError::NotFound)?)).collect()
        }).await
    }
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
        self.execute_read(move |c| {
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
                    "SELECT r.ordinal,r.root_run_id FROM ef_history_runs r WHERE r.session_id=?1 AND r.ordinal<?2 AND NOT EXISTS(SELECT 1 FROM ef_history_deleted_turns d WHERE d.session_id=r.session_id AND d.root_run_id=r.root_run_id) AND NOT EXISTS(SELECT 1 FROM ef_history_runs p WHERE p.session_id=r.session_id AND p.root_run_id=r.root_run_id AND p.ordinal<r.ordinal) ORDER BY r.ordinal DESC LIMIT ?3"
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
            // Page roots bound every collection query. Never load an entire
            // session or issue four more SELECTs for each run in the page.
            let roots = encode(&roots.into_iter().map(|(_, root)| root).collect::<Vec<_>>())?;
            let runs = {
                let mut q = tx.prepare(
                    "SELECT r.record FROM json_each(?2) page JOIN ef_history_runs r ON r.root_run_id=page.value AND r.session_id=?1 AND NOT EXISTS(SELECT 1 FROM ef_history_deleted_turns d WHERE d.session_id=r.session_id AND d.root_run_id=r.root_run_id) ORDER BY CAST(page.key AS INTEGER),r.ordinal"
                ).map_err(sql)?;
                q.query_map(params![session_id, roots], |row| row.get(0)).map_err(sql)?
                    .map(|row| decode(row.map_err(sql)?)).collect::<Result<Vec<HistoryRun>, _>>()?
            };
            let can_delete_turns = deletion::supported(&tx, &session_id)?;
            let history = HistoryRecords {
                steps: page_records(&tx, "ef_history_steps", &session_id, &roots)?,
                messages: page_records(&tx, "ef_history_messages", &session_id, &roots)?,
                tool_invocations: page_records(&tx, "ef_history_tools", &session_id, &roots)?,
                interactions: page_records(&tx, "ef_history_interactions", &session_id, &roots)?,
            };
            let pending_interactions = {
                let mut q = tx.prepare(
                    "SELECT i.record FROM ef_history_interactions i JOIN ef_history_runs r ON r.id=i.run_id WHERE r.session_id=?1 AND r.active=1 AND i.answered=0 ORDER BY r.ordinal,i.position"
                ).map_err(sql)?;
                q.query_map([&session_id], |r| r.get(0)).map_err(sql)?
                    .map(|r| decode(r.map_err(sql)?)).collect::<Result<Vec<HistoryInteraction>, _>>()?
            };
            let active_runs = {
                let mut q = tx.prepare(
                    "SELECT record FROM ef_history_runs WHERE session_id=?1 AND active=1 ORDER BY ordinal"
                ).map_err(sql)?;
                q.query_map([&session_id], |r| r.get(0)).map_err(sql)?
                    .map(|r| decode(r.map_err(sql)?)).collect::<Result<Vec<HistoryRun>, _>>()?
            };
            tx.commit().map_err(sql)?;
            Ok(SessionHistory {
                can_delete_turns, session_id, version, next_cursor, runs, records: history, pending_interactions, active_runs,
            })
        }).await
    }
}
