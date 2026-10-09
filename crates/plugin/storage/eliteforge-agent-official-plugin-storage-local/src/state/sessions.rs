use super::*;

fn has_table(connection: &rusqlite::Connection, name: &str) -> Result<bool, StateError> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
            [name],
            |row| row.get(0),
        )
        .map_err(database_error)
}

#[async_trait]
impl SessionStore for SqliteStateStore {
    async fn delete_session(&self, session_id: &str) -> Result<bool, StateError> {
        if session_id.trim().is_empty() {
            return Err(StateError::Invalid("session ID must be nonempty".into()));
        }
        let session_id = session_id.to_owned();
        let result = self.execute(move |connection| {
            // One transaction per session: a failure rolls back only this
            // identity, while the batch caller continues with the remaining IDs.
            let tx = crate::database::begin(connection)
                .map_err(database_error)?;
            // The execution-only plugin need not install conversations/summaries.
            let conversations = has_table(&tx, "ef_conversations")?;
            let summaries = has_table(&tx, "ef_context_summaries")?;
            let now = db_now(&tx)?;
            let busy: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM ef_checkpoints WHERE session_id=?1 AND owner IS NOT NULL AND expires>?2)",
                params![session_id, now], |row| row.get(0),
            ).map_err(database_error)?;
            if busy {
                return Err(StateError::Busy);
            }
            if crate::lifecycle::busy(&tx, &session_id)? { return Err(StateError::Busy); }
            let mut identities = vec![session_id.clone()];
            {
                let mut query = tx.prepare("SELECT id FROM ef_sessions WHERE owner_session_id=?1 AND id<>?1").map_err(database_error)?;
                identities.extend(query.query_map([&session_id], |r| r.get::<_, String>(0)).map_err(database_error)?.collect::<Result<Vec<_>,_>>().map_err(database_error)?);
            }
            let mut total = 0;
            for session_id in identities {
                let busy: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM ef_checkpoints WHERE session_id=?1 AND owner IS NOT NULL AND expires>?2)",params![session_id,now],|r|r.get(0)).map_err(database_error)?;
                if busy || crate::lifecycle::busy(&tx, &session_id)? { return Err(StateError::Busy); }
            if conversations {
                let busy: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM ef_conversations WHERE session=?1 AND owner IS NOT NULL AND lease_until>?2)",
                    params![session_id, now], |row| row.get(0),
                ).map_err(database_error)?;
                if busy {
                    return Err(StateError::Busy);
                }
            }
            // Remove the foreign-key head before its checkpoints. All runs
            // and all branches belong to the same session deletion boundary.
            let mut rows = tx.execute("DELETE FROM ef_session_heads WHERE session_id=?1", [&session_id]).map_err(database_error)?;
            rows += tx.execute("DELETE FROM ef_checkpoints WHERE session_id=?1", [&session_id]).map_err(database_error)?;
            rows += tx.execute("DELETE FROM ef_interactions WHERE session_id=?1", [&session_id]).map_err(database_error)?;
            if conversations {
                rows += tx.execute("DELETE FROM ef_conversations WHERE session=?1", [&session_id]).map_err(database_error)?;
            }
            if summaries {
                // Keys are JSON-encoded SummaryKey values. Compare the exact
                // session field, never a prefix or a LIKE pattern.
                rows += tx.execute(
                    "DELETE FROM ef_context_summaries WHERE json_extract(key, '$.conversation.session_id')=?1",
                    [&session_id],
                ).map_err(database_error)?;
            }
            for table in ["ef_history_steps", "ef_history_messages", "ef_history_tools", "ef_history_interactions", "ef_history_runs", "ef_history_versions", "ef_events", "ef_session_artifact_refs", "ef_session_context", "ef_runs"] {
                rows += tx.execute(&format!("DELETE FROM {table} WHERE session_id=?1"), [&session_id]).map_err(database_error)?;
            }
            tx.execute("INSERT OR IGNORE INTO ef_artifact_cleanup(artifact_id) SELECT artifact_id FROM ef_artifact_ownership WHERE NOT EXISTS(SELECT 1 FROM ef_session_artifact_refs r WHERE r.artifact_id=ef_artifact_ownership.artifact_id)",[]).map_err(database_error)?;
            if has_table(&tx,"ef_local_artifact_metadata")? { tx.execute("DELETE FROM ef_local_artifact_metadata WHERE id IN (SELECT artifact_id FROM ef_artifact_cleanup)",[]).map_err(database_error)?; }
            rows += tx.execute("DELETE FROM ef_goals WHERE session_id=?1", [&session_id]).map_err(database_error)?;
            rows += tx.execute("DELETE FROM ef_sessions WHERE id=?1", [&session_id]).map_err(database_error)?;
            // Global admission ledger prevents a late writer recreating a deleted identity.
            if rows != 0 { tx.execute("INSERT INTO ef_session_ids(id,deleted) VALUES (?1,1) ON CONFLICT(id) DO UPDATE SET deleted=1", [&session_id]).map_err(database_error)?; }
            total += rows;
            }
            tx.commit().map_err(database_error)?;
            Ok(total != 0)
        }).await?;
        let store = self.clone();
        self.connection.after_commit(async move {
            store.cleanup_artifacts().await;
        });
        Ok(result)
    }
}

impl SqliteStateStore {
    pub(crate) async fn cleanup_artifacts(&self) {
        if let Some(files) = &self.artifact_files {
            crate::lifecycle::cleanup_artifacts(&self.connection, files).await;
        }
    }
}
