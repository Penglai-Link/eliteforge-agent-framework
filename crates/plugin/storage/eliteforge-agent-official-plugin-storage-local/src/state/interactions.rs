use super::*;

#[derive(Clone)]
pub struct SqliteExecutionPlugin(pub Arc<SqliteStateStore>);
#[async_trait]
impl Plugin for SqliteExecutionPlugin {
    fn id(&self) -> &str {
        "sqlite.execution"
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let state_store: Arc<dyn StateStore> = self.0.clone();
        let interaction_store: Arc<dyn InteractionStore> = self.0.clone();
        context.provide(StateStoreService(state_store.clone()))?;
        context.contribute_extension(state_store)?;
        context.provide(InteractionStoreService(interaction_store.clone()))?;
        context.contribute_extension(interaction_store)?;
        context.provide(SessionStoreService(self.0.clone()))?;
        context.provide(HistoryStoreService(self.0.clone()))?;
        context.contribute_extension(self.0.clone() as Arc<dyn SessionStore>)
    }
}

fn read_record(
    connection: &rusqlite::Connection,
    session: &str,
    id: RunId,
    max: usize,
) -> Result<InteractionRecord, StateError> {
    let encoded: String = connection.query_row(
        "SELECT record FROM ef_interactions WHERE id=?1 AND session_id=?2 AND length(CAST(record AS BLOB))<=?3",
        params![id.to_string(), session, max], |row| row.get(0),
    ).optional().map_err(database_error)?.ok_or(StateError::NotFound)?;
    serde_json::from_str(&encoded)
        .map_err(|_| StateError::Invalid("invalid interaction record".into()))
}
fn encode_record(record: &InteractionRecord, max: usize) -> Result<String, StateError> {
    let encoded = serde_json::to_string(record)
        .map_err(|_| StateError::Invalid("invalid interaction record".into()))?;
    if encoded.len() > max {
        return Err(StateError::Invalid(
            "interaction exceeds storage limit".into(),
        ));
    }
    Ok(encoded)
}
#[async_trait]
impl InteractionStore for SqliteStateStore {
    async fn publish(&self, record: &InteractionRecord) -> Result<(), StateError> {
        if record.request.id.is_nil()
            || record.run_id.is_nil()
            || record.session_id.trim().is_empty()
            || record.response.is_some()
            || record.answered_at_unix_ms.is_some()
        {
            return Err(StateError::Invalid("invalid pending interaction".into()));
        }
        compile_schema(&record.request.response_schema)
            .map_err(|_| StateError::Invalid("invalid interaction schema".into()))?;
        if let InteractionKind::Approval { invocation_id } = record.request.kind
            && record
                .tool_call
                .as_ref()
                .is_none_or(|call| call.invocation_id != invocation_id)
        {
            return Err(StateError::Invalid(
                "approval must bind the original tool invocation".into(),
            ));
        }
        let record = record.clone();
        let encoded = encode_record(&record, self.max_checkpoint_bytes)?;
        let max = self.max_checkpoint_bytes;
        self.execute(move |connection| {
            let tx = crate::database::begin(connection).map_err(database_error)?;
            let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM ef_interactions WHERE id=?1)", [record.request.id.to_string()], |r| r.get(0)).map_err(database_error)?;
            if exists {
                let previous = read_record(&tx, &record.session_id, record.request.id, max)?;
                if previous.run_id != record.run_id || previous.node_invocation_id != record.node_invocation_id
                    || previous.request != record.request || previous.tool_call != record.tool_call {
                    return Err(StateError::Conflict);
                }
            } else {
                tx.execute("INSERT INTO ef_interactions(id,session_id,run_id,answered,record) VALUES (?1,?2,?3,0,?4)",
                    params![record.request.id.to_string(), record.session_id, record.run_id.to_string(), encoded]).map_err(database_error)?;
            }
            tx.commit().map_err(database_error)
        }).await
    }
    async fn get(&self, session: &str, id: RunId) -> Result<InteractionRecord, StateError> {
        let session = session.to_owned();
        let max = self.max_checkpoint_bytes;
        self.execute(move |c| read_record(c, &session, id, max))
            .await
    }
    async fn pending(&self, session: &str) -> Result<Vec<InteractionRecord>, StateError> {
        let session = session.to_owned();
        let max = self.max_checkpoint_bytes;
        self.execute(move |c| {
            let mut query = c.prepare("SELECT record FROM ef_interactions WHERE session_id=?1 AND answered=0 ORDER BY rowid LIMIT 1025").map_err(database_error)?;
            let rows = query.query_map([session], |r| r.get::<_, String>(0)).map_err(database_error)?;
            let mut result = Vec::new(); let mut bytes = 0usize;
            for row in rows {
                let encoded = row.map_err(database_error)?; bytes = bytes.saturating_add(encoded.len());
                if bytes > max || result.len() >= 1024 { return Err(StateError::Invalid("pending interactions exceed query limit".into())); }
                result.push(serde_json::from_str(&encoded).map_err(|_| StateError::Invalid("invalid interaction record".into()))?);
            }
            Ok(result)
        }).await
    }
    async fn answer(
        &self,
        session: &str,
        response: &InteractionResponse,
    ) -> Result<InteractionRecord, AgentError> {
        let session = session.to_owned();
        let response = response.clone();
        let max = self.max_checkpoint_bytes;
        // Validation and the pending -> answered transition share one write transaction.
        Ok(self
            .execute(move |c| {
                let tx = crate::database::begin(c).map_err(database_error)?;
                let mut record = read_record(&tx, &session, response.interaction_id, max)?;
                if let Some(previous) = &record.response {
                    return if previous == &response {
                        Ok(record)
                    } else {
                        Err(StateError::Conflict)
                    };
                }
                let now = db_now(&tx)?;
                record
                    .request
                    .validate_response(&response, now)
                    .map_err(|_| {
                        StateError::Invalid(
                            "answer does not match the pending request or it has expired".into(),
                        )
                    })?;
                record.response = Some(response);
                record.answered_at_unix_ms = Some(now);
                let encoded = encode_record(&record, max)?;
                tx.execute(
                    "UPDATE ef_interactions SET answered=1, record=?1 WHERE id=?2 AND answered=0",
                    params![encoded, record.request.id.to_string()],
                )
                .map_err(database_error)?;
                crate::history::answer(&tx, &record)?;
                tx.commit().map_err(database_error)?;
                Ok(record)
            })
            .await?)
    }
}
