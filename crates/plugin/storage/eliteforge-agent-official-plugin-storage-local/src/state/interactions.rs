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
pub(super) fn close_run(
    c: &rusqlite::Connection,
    session: &str,
    run: RunId,
) -> Result<(), StateError> {
    c.execute(
        "INSERT OR IGNORE INTO ef_closed_interaction_runs(session_id,run_id) VALUES (?1,?2)",
        params![session, run.to_string()],
    )
    .map_err(database_error)?;
    Ok(())
}

#[async_trait]
impl InteractionStore for SqliteStateStore {
    async fn publish(&self, record: &InteractionRecord) -> Result<(), StateError> {
        self.publish_many(std::slice::from_ref(record)).await
    }
    async fn publish_many(&self, records: &[InteractionRecord]) -> Result<(), StateError> {
        if records.is_empty() {
            return Ok(());
        }
        let mut encoded = Vec::with_capacity(records.len());
        for record in records {
            if record.request.id.is_nil()
                || record.run_id.is_nil()
                || record.session_id.trim().is_empty()
                || record.response.is_some()
                || record.response_provenance.is_some()
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
            encoded.push((
                record.clone(),
                encode_record(record, self.max_checkpoint_bytes)?,
            ));
        }
        let ids = serde_json::to_string(
            &records
                .iter()
                .map(|record| record.request.id.to_string())
                .collect::<Vec<_>>(),
        )
        .map_err(|e| StateError::Invalid(e.to_string()))?;
        let max = self.max_checkpoint_bytes;
        self.execute(move |connection| {
            let tx = crate::database::begin(connection).map_err(database_error)?;
            let mut previous = std::collections::HashMap::<RunId,InteractionRecord>::new();
            {
                let mut q = tx.prepare("SELECT record FROM ef_interactions WHERE id IN (SELECT value FROM json_each(?1))").map_err(database_error)?;
                let rows = q.query_map([ids],|r|r.get::<_,String>(0)).map_err(database_error)?;
                for row in rows {
                    let raw = row.map_err(database_error)?;
                    if raw.len() > max { return Err(StateError::NotFound); }
                    let record: InteractionRecord = serde_json::from_str(&raw).map_err(|_| StateError::Invalid("invalid interaction record".into()))?;
                    previous.insert(record.request.id,record);
                }
            }
            for (record,encoded) in encoded {
                if let Some(old) = previous.get(&record.request.id) {
                    if old.session_id != record.session_id || old.run_id != record.run_id
                        || old.request != record.request || old.tool_call != record.tool_call {
                        return Err(StateError::Conflict);
                    }
                } else {
                    tx.execute("INSERT INTO ef_interactions(id,session_id,run_id,answered,record) VALUES (?1,?2,?3,0,?4)",params![record.request.id.to_string(),record.session_id,record.run_id.to_string(),encoded]).map_err(database_error)?;
                    previous.insert(record.request.id,record);
                }
            }
            tx.commit().map_err(database_error)
        }).await
    }
    async fn get(&self, session: &str, id: RunId) -> Result<InteractionRecord, StateError> {
        let session = session.to_owned();
        let max = self.max_checkpoint_bytes;
        self.execute_read(move |c| read_record(c, &session, id, max))
            .await
    }
    async fn get_many(
        &self,
        session: &str,
        ids: &[RunId],
    ) -> Result<Vec<InteractionRecord>, StateError> {
        self.find_many(session, ids)
            .await?
            .into_iter()
            .map(|record| record.ok_or(StateError::NotFound))
            .collect()
    }
    async fn find_many(
        &self,
        session: &str,
        ids: &[RunId],
    ) -> Result<Vec<Option<InteractionRecord>>, StateError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let session = session.to_owned();
        let ids = serde_json::to_string(&ids.iter().map(ToString::to_string).collect::<Vec<_>>())
            .map_err(|_| StateError::Invalid("invalid interaction identities".into()))?;
        let max = self.max_checkpoint_bytes;
        self.execute_read(move |c| {
            let mut q = c.prepare("SELECT i.record FROM json_each(?1) requested LEFT JOIN ef_interactions i ON i.id=requested.value AND i.session_id=?2 AND length(CAST(i.record AS BLOB))<=?3 ORDER BY CAST(requested.key AS INTEGER)").map_err(database_error)?;
            q.query_map(params![ids,session,max], |r|r.get::<_,Option<String>>(0)).map_err(database_error)?
                .map(|row| row.map_err(database_error)?.map(|encoded| serde_json::from_str(&encoded).map_err(|_| StateError::Invalid("invalid interaction record".into()))).transpose()).collect()
        }).await
    }
    async fn close_run(&self, session: &str, run: RunId) -> Result<(), StateError> {
        let session = session.to_owned();
        self.execute(move |c| close_run(c, &session, run)).await
    }
    async fn pending(&self, session: &str) -> Result<Vec<InteractionRecord>, StateError> {
        let session = session.to_owned();
        let max = self.max_checkpoint_bytes;
        self.execute_read(move |c| {
            let mut query = c.prepare("SELECT i.record FROM ef_interactions i WHERE i.session_id=?1 AND i.answered=0 AND NOT EXISTS (SELECT 1 FROM ef_closed_interaction_runs closed WHERE closed.session_id=i.session_id AND closed.run_id=i.run_id) ORDER BY i.rowid LIMIT 1025").map_err(database_error)?;
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
        self.answer_with_provenance(session, response, None).await
    }
    async fn answer_with_provenance(
        &self,
        session: &str,
        response: &InteractionResponse,
        provenance: Option<&MessageProvenance>,
    ) -> Result<InteractionRecord, AgentError> {
        let session = session.to_owned();
        let response = response.clone();
        let provenance = provenance.cloned();
        let max = self.max_checkpoint_bytes;
        // Validation and the pending -> answered transition share one write transaction.
        Ok(self
            .execute(move |c| {
                let tx = crate::database::begin(c).map_err(database_error)?;
                let mut record = read_record(&tx, &session, response.interaction_id, max)?;
                if let Some(previous) = &record.response {
                    if previous != &response
                        || (provenance.is_some()
                            && record.response_provenance.is_some()
                            && provenance != record.response_provenance)
                    {
                        return Err(StateError::Conflict);
                    }
                    if provenance.is_none() || record.response_provenance.is_some() {
                        return Ok(record);
                    }
                    // The legacy answer API can commit before the host supplies
                    // its trusted sender on the resume request. Fill it once.
                } else {
                    let closed: bool = tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM ef_closed_interaction_runs WHERE session_id=?1 AND run_id=?2)",
                        params![session, record.run_id.to_string()], |r| r.get(0)).map_err(database_error)?;
                    if closed { return Err(StateError::Invalid("interaction run is terminal".into())); }
                    let now = db_now(&tx)?;
                    record
                        .request
                        .validate_response(&response, now)
                        .map_err(|_| {
                            StateError::Invalid(
                                "answer does not match the pending request or it has expired"
                                    .into(),
                            )
                        })?;
                    record.response = Some(response);
                    record.answered_at_unix_ms = Some(now);
                }
                record.response_provenance = provenance;
                let encoded = encode_record(&record, max)?;
                tx.execute(
                    "UPDATE ef_interactions SET answered=1, record=?1 WHERE id=?2",
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
