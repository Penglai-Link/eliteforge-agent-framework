//! Transactional turn tombstones and active-context reconstruction.
use super::{decode, encode};
use crate::lifecycle::sql;
use eliteforge_agent_core::*;
use rusqlite::{OptionalExtension, params};

pub(super) fn supported(c: &rusqlite::Connection, session: &str) -> Result<bool, StateError> {
    c.query_row("SELECT NOT EXISTS(SELECT 1 FROM ef_history_messages m WHERE m.session_id=?1 AND json_extract(m.record,'$.role')='User' AND NOT EXISTS(SELECT 1 FROM ef_history_transcript_spans s WHERE s.run_id=m.run_id) AND NOT EXISTS(SELECT 1 FROM ef_checkpoints p WHERE p.run_id=m.run_id AND CASE WHEN json_valid(p.checkpoint) THEN json_extract(p.checkpoint,'$.loop_id') END='framework.react') AND NOT EXISTS(SELECT 1 FROM ef_history_runs r JOIN ef_history_deleted_turns d ON d.session_id=r.session_id AND d.root_run_id=r.root_run_id WHERE r.id=m.run_id))", [session], |r| r.get(0)).map_err(sql)
}

pub(super) fn revision(c: &rusqlite::Connection, session: &str) -> Result<String, StateError> {
    c.query_row("SELECT revision FROM ef_history_deleted_turns WHERE session_id=?1 ORDER BY rowid DESC LIMIT 1", [session], |r| r.get(0))
        .optional().map_err(sql).map(|v| v.unwrap_or_else(|| "0".into()))
}

pub(super) fn record_span(
    c: &rusqlite::Connection,
    checkpoint: &Checkpoint,
) -> Result<(), StateError> {
    if let Some(span) = &checkpoint.conversation_span {
        c.execute("INSERT INTO ef_history_transcript_spans VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(run_id) DO UPDATE SET end=excluded.end WHERE generation=excluded.generation AND start=excluded.start",
            params![checkpoint.run_id.to_string(), checkpoint.session_id, span.key.branch, span.generation, span.start, checkpoint.transcript.len()]).map_err(sql)?;
    }
    Ok(())
}

fn has_table(c: &rusqlite::Connection, name: &str) -> Result<bool, StateError> {
    c.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name=?1)",
        [name],
        |r| r.get(0),
    )
    .map_err(sql)
}

/// Existing durable ReAct checkpoints also describe exact cumulative prefixes.
/// Resolve their ownership before invalidating checkpoints; never match by text.
fn resolve_spans(c: &rusqlite::Connection, session: &str) -> Result<(), StateError> {
    let missing: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM ef_checkpoints p WHERE p.session_id=?1 AND CASE WHEN json_valid(p.checkpoint) THEN json_extract(p.checkpoint,'$.loop_id') END='framework.react' AND NOT EXISTS(SELECT 1 FROM ef_history_transcript_spans s WHERE s.run_id=p.run_id))", [session], |r| r.get(0)).map_err(sql)?;
    if !missing {
        return Ok(());
    }
    let mut q = c.prepare("SELECT c.checkpoint FROM ef_checkpoints c JOIN ef_history_runs r ON r.id=c.run_id WHERE c.session_id=?1 ORDER BY r.ordinal").map_err(sql)?;
    let checkpoints = q
        .query_map([session], |r| r.get::<_, String>(0))
        .map_err(sql)?;
    let mut prefixes = std::collections::HashMap::<(String, String), Vec<Message>>::new();
    for encoded in checkpoints {
        let mut checkpoint: Checkpoint = decode(encoded.map_err(sql)?)?;
        if checkpoint.loop_id != "framework.react" {
            continue;
        }
        let Some(generation) = checkpoint
            .state
            .get("conversation_generation")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
        else {
            continue;
        };
        let branch = checkpoint
            .state
            .pointer("/request/conversation_branch")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("main")
            .to_owned();
        let key = (branch.clone(), generation.clone());
        let previous = prefixes.entry(key).or_default();
        if !checkpoint.transcript.starts_with(previous) {
            return Err(StateError::Conflict);
        }
        if checkpoint.conversation_span.is_none() {
            checkpoint.conversation_span = Some(ConversationSpan {
                key: ConversationKey::new(session).branch(branch),
                generation,
                start: previous.len(),
            });
        }
        record_span(c, &checkpoint)?;
        *previous = checkpoint.transcript;
    }
    Ok(())
}

pub(super) fn delete(
    c: &mut rusqlite::Connection,
    session: &str,
    root: RunId,
    expected: &str,
) -> Result<String, StateError> {
    let tx = crate::database::begin(c).map_err(sql)?;
    crate::lifecycle::require_session(&tx, session, true)?;
    if !supported(&tx, session)? {
        return Err(StateError::UnsupportedVersion);
    }
    let root = root.to_string();
    // A retry after an ambiguous network failure is harmless.
    if tx.query_row("SELECT EXISTS(SELECT 1 FROM ef_history_deleted_turns WHERE session_id=?1 AND root_run_id=?2)", params![session, root], |r| r.get::<_, bool>(0)).map_err(sql)? {
        return revision(&tx, session);
    }
    let version: String = tx
        .query_row(
            "SELECT version FROM ef_history_versions WHERE session_id=?1",
            [session],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql)?
        .unwrap_or_else(|| "0".into());
    if version != expected {
        return Err(StateError::Conflict);
    }
    let exists: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM ef_history_runs WHERE session_id=?1 AND root_run_id=?2)",
            params![session, root],
            |r| r.get(0),
        )
        .map_err(sql)?;
    if !exists {
        return Err(StateError::NotFound);
    }
    let revision = RunId::new_v4().to_string();
    {
        // Lineage is not authorization to edit a different session.
        // Pending input and sibling/child activity also prevent a rebase.
        if crate::lifecycle::busy(&tx, session)? || tx.query_row("SELECT EXISTS(SELECT 1 FROM ef_history_runs WHERE (session_id=?1 OR owner_session_id=?1) AND active=1)", [session], |r| r.get::<_, bool>(0)).map_err(sql)? {
            return Err(StateError::Busy);
        }
        resolve_spans(&tx, session)?;
        if has_table(&tx, "ef_conversations")? {
            rebase(&tx, session, &root)?;
        }
        tx.execute(
            "INSERT INTO ef_history_deleted_turns VALUES (?1,?2,?3)",
            params![session, root, revision],
        )
        .map_err(sql)?;
        // All checkpoints may contain the deleted prefix. A new run starts from
        // the rebased conversation; historical run/event audit remains intact.
        for table in ["ef_session_heads", "ef_checkpoints", "ef_interactions"] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE session_id=?1"),
                [session],
            )
            .map_err(sql)?;
        }
        if has_table(&tx, "ef_context_summaries")? {
            tx.execute("DELETE FROM ef_context_summaries WHERE json_extract(key,'$.conversation.session_id')=?1", [session]).map_err(sql)?;
        }
        super::touch(&tx, session)?;
    }
    tx.commit().map_err(sql)?;
    Ok(revision)
}

fn rebase(c: &rusqlite::Connection, session: &str, root: &str) -> Result<(), StateError> {
    // Select only affected generations, then read all their spans in one query.
    // All reads and rewrites remain inside delete()'s transaction and busy fence.
    let mut spans_by_branch =
        std::collections::BTreeMap::<(String, String), Vec<(String, usize, usize, bool)>>::new();
    {
        let mut q = c.prepare("SELECT s.branch,s.generation,s.run_id,s.start,s.end,r.root_run_id=?2 FROM ef_history_transcript_spans s JOIN ef_history_runs r ON r.id=s.run_id JOIN ef_conversations v ON v.session=s.session_id AND v.branch=s.branch AND v.generation=s.generation WHERE s.session_id=?1 AND EXISTS(SELECT 1 FROM ef_history_transcript_spans d JOIN ef_history_runs dr ON dr.id=d.run_id WHERE d.session_id=s.session_id AND d.branch=s.branch AND d.generation=s.generation AND dr.root_run_id=?2) ORDER BY s.branch,s.generation,s.start").map_err(sql)?;
        let rows = q
            .query_map(params![session, root], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })
            .map_err(sql)?;
        for row in rows {
            let (branch, generation, run, start, end, deleted) = row.map_err(sql)?;
            spans_by_branch
                .entry((branch, generation))
                .or_default()
                .push((run, start, end, deleted));
        }
    }
    let branches: Vec<(String, String, String)> = {
        let mut q = c.prepare("SELECT v.branch,v.generation,v.messages FROM ef_conversations v WHERE v.session=?1 AND EXISTS(SELECT 1 FROM ef_history_transcript_spans s JOIN ef_history_runs r ON r.id=s.run_id WHERE s.session_id=v.session AND s.branch=v.branch AND s.generation=v.generation AND r.root_run_id=?2)").map_err(sql)?;
        q.query_map(params![session, root], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .map_err(sql)?
        .collect::<Result<_, _>>()
        .map_err(sql)?
    };
    for (branch, generation, data) in branches {
        let mut messages: Vec<Message> = decode(data)?;
        let spans = spans_by_branch
            .remove(&(branch.clone(), generation))
            .unwrap_or_default();
        // Unrelated conversation branches are unaffected.
        if !spans.iter().any(|(_, _, _, deleted)| *deleted) {
            continue;
        }
        let mut removed = vec![false; messages.len()];
        let mut previous_end = 0;
        for (_, start, end, deleted) in &spans {
            if start > end || *start < previous_end {
                return Err(StateError::Conflict);
            }
            previous_end = *end;
            if *end > messages.len() {
                return Err(StateError::Conflict);
            }
            if *deleted {
                removed[*start..*end].fill(true);
            }
        }
        let mut offsets = Vec::with_capacity(removed.len() + 1);
        offsets.push(0usize);
        for deleted in &removed {
            offsets.push(offsets.last().copied().unwrap() + usize::from(!deleted));
        }
        let mut index = 0;
        messages.retain(|_| {
            let keep = !removed[index];
            index += 1;
            keep
        });
        validate_messages(&messages).map_err(|e| StateError::Invalid(e.to_string()))?;
        let next = RunId::new_v4().to_string();
        c.execute("UPDATE ef_conversations SET messages=?3,revision=?4,generation=?5,owner=NULL,lease_until=0 WHERE session=?1 AND branch=?2", params![session, branch, encode(&messages)?, messages.len(), next]).map_err(sql)?;
        for (run, start, end, deleted) in spans {
            if deleted {
                c.execute(
                    "DELETE FROM ef_history_transcript_spans WHERE run_id=?1",
                    [run],
                )
                .map_err(sql)?;
            } else {
                c.execute("UPDATE ef_history_transcript_spans SET start=?2,end=?3,generation=?4 WHERE run_id=?1", params![run, offsets[start], offsets[end], next]).map_err(sql)?;
            }
        }
    }
    Ok(())
}
