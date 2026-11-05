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
            let identities = {
                let mut q = tx.prepare("SELECT ?1 UNION SELECT id FROM ef_sessions WHERE owner_session_id=?1").map_err(database_error)?;
                q.query_map([&session_id],|r|r.get::<_,String>(0)).map_err(database_error)?.collect::<Result<Vec<_>,_>>().map_err(database_error)?
            };
            let identities = serde_json::to_string(&identities).map_err(|e| StateError::Invalid(e.to_string()))?;
            let busy: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM ef_checkpoints WHERE session_id IN (SELECT value FROM json_each(?1)) AND owner IS NOT NULL AND expires>?2) OR EXISTS(SELECT 1 FROM ef_runs WHERE (session_id IN (SELECT value FROM json_each(?1)) OR owner_session_id IN (SELECT value FROM json_each(?1))) AND status='RUNNING' AND lease_until>?2)",
                params![identities,now],|r|r.get(0),
            ).map_err(database_error)?;
            if busy { return Err(StateError::Busy); }
            if conversations {
                let busy: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM ef_conversations WHERE session IN (SELECT value FROM json_each(?1)) AND owner IS NOT NULL AND lease_until>?2)",params![identities,now],|r|r.get(0)).map_err(database_error)?;
                if busy { return Err(StateError::Busy); }
            }
            // Fixed work per table, independent of the number of owned sessions.
            let mut tables = vec![
                ("ef_session_heads","session_id"), ("ef_checkpoints","session_id"),
                ("ef_interactions","session_id"), ("ef_closed_interaction_runs","session_id"), ("ef_history_steps","session_id"),
                ("ef_history_messages","session_id"), ("ef_history_tools","session_id"),
                ("ef_history_interactions","session_id"), ("ef_history_runs","session_id"),
                ("ef_history_versions","session_id"), ("ef_history_deleted_turns","session_id"), ("ef_history_transcript_spans","session_id"), ("ef_events","session_id"),
                ("ef_session_artifact_refs","session_id"), ("ef_session_context","session_id"),
                ("ef_runs","session_id"), ("ef_goals","session_id"), ("ef_sessions","id"),
            ];
            if conversations { tables.insert(3,("ef_conversations","session")); }
            if summaries { tables.insert(3,("ef_context_summaries","json_extract(key, '$.conversation.session_id')")); }
            // Tombstone only identities that actually own data, including orphan
            // checkpoint rows; deleting an absent ID remains a no-op.
            let union = tables.iter().map(|(table,column)| format!("SELECT {column} AS id FROM {table} WHERE {column} IN (SELECT value FROM json_each(?1))")).collect::<Vec<_>>().join(" UNION ");
            let present = {
                let mut q = tx.prepare(&union).map_err(database_error)?;
                q.query_map([&identities],|r|r.get::<_,String>(0)).map_err(database_error)?.collect::<Result<Vec<_>,_>>().map_err(database_error)?
            };
            let mut total = 0;
            for (table,column) in tables {
                total += tx.execute(&format!("DELETE FROM {table} WHERE {column} IN (SELECT value FROM json_each(?1))"),[&identities]).map_err(database_error)?;
            }
            tx.execute("INSERT OR IGNORE INTO ef_artifact_cleanup(artifact_id) SELECT artifact_id FROM ef_artifact_ownership WHERE NOT EXISTS(SELECT 1 FROM ef_session_artifact_refs r WHERE r.artifact_id=ef_artifact_ownership.artifact_id)",[]).map_err(database_error)?;
            if has_table(&tx,"ef_local_artifact_metadata")? {
                tx.execute("DELETE FROM ef_local_artifact_metadata WHERE id IN (SELECT artifact_id FROM ef_artifact_cleanup)",[]).map_err(database_error)?;
            }
            let present = serde_json::to_string(&present).map_err(|e| StateError::Invalid(e.to_string()))?;
            tx.execute("INSERT INTO ef_session_ids(id,deleted) SELECT value,1 FROM json_each(?1) WHERE true ON CONFLICT(id) DO UPDATE SET deleted=1",[present]).map_err(database_error)?;
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
