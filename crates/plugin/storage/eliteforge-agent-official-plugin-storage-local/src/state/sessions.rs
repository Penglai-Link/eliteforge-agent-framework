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
        self.execute(move |connection| {
            // One transaction per session: a failure rolls back only this
            // identity, while the batch caller continues with the remaining IDs.
            let tx = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(database_error)?;
            // The execution-only plugin need not install conversations/summaries.
            let conversations = has_table(&tx, "conversations")?;
            let summaries = has_table(&tx, "context_summaries")?;
            let now = db_now(&tx)?;
            let busy: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM agent_checkpoints WHERE session_id=?1 AND owner IS NOT NULL AND expires>?2)",
                params![session_id, now], |row| row.get(0),
            ).map_err(database_error)?;
            if busy {
                return Err(StateError::Busy);
            }
            if conversations {
                let busy: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM conversations WHERE session=?1 AND owner IS NOT NULL AND lease_until>?2)",
                    params![session_id, now], |row| row.get(0),
                ).map_err(database_error)?;
                if busy {
                    return Err(StateError::Busy);
                }
            }
            // Remove the foreign-key head before its checkpoints. All runs
            // and all branches belong to the same session deletion boundary.
            let mut rows = tx.execute("DELETE FROM agent_session_heads WHERE session_id=?1", [&session_id]).map_err(database_error)?;
            rows += tx.execute("DELETE FROM agent_checkpoints WHERE session_id=?1", [&session_id]).map_err(database_error)?;
            rows += tx.execute("DELETE FROM agent_interactions WHERE session_id=?1", [&session_id]).map_err(database_error)?;
            if conversations {
                rows += tx.execute("DELETE FROM conversations WHERE session=?1", [&session_id]).map_err(database_error)?;
            }
            if summaries {
                // Keys are JSON-encoded SummaryKey values. Compare the exact
                // session field, never a prefix or a LIKE pattern.
                rows += tx.execute(
                    "DELETE FROM context_summaries WHERE json_extract(key, '$.conversation.session_id')=?1",
                    [&session_id],
                ).map_err(database_error)?;
            }
            tx.commit().map_err(database_error)?;
            Ok(rows != 0)
        }).await
    }
}
