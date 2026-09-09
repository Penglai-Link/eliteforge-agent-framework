use async_trait::async_trait;
use eliteforge_agent_core::*;
use std::{
    path::Path,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::Semaphore;
use tokio_rusqlite::{
    Connection,
    rusqlite::{self, OptionalExtension, params},
};

#[derive(Debug, Clone)]
pub struct MemoryStoreOptions {
    pub max_entries: usize,
    pub max_entry_bytes: usize,
    pub max_search_results: usize,
    pub max_search_bytes: usize,
    pub max_pending_operations: usize,
}
impl Default for MemoryStoreOptions {
    fn default() -> Self {
        Self {
            max_entries: 10_000,
            max_entry_bytes: 16 * 1024,
            max_search_results: 100,
            max_search_bytes: 256 * 1024,
            max_pending_operations: 64,
        }
    }
}
impl MemoryStoreOptions {
    fn validate(&self) -> Result<(), MemoryError> {
        if [
            self.max_entries,
            self.max_entry_bytes,
            self.max_search_results,
            self.max_search_bytes,
            self.max_pending_operations,
        ]
        .contains(&0)
            || self.max_pending_operations > Semaphore::MAX_PERMITS
        {
            return Err(invalid("memory limits must be positive and bounded"));
        }
        Ok(())
    }
    fn validate_query(&self, namespace: &str, query: &MemoryQuery) -> Result<(), MemoryError> {
        validate_namespace(namespace)?;
        if query.text.len() > 2048
            || query.limit == 0
            || query.limit > self.max_search_results
            || query.max_bytes == 0
            || query.max_bytes > self.max_search_bytes
        {
            return Err(invalid("invalid memory search bounds"));
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct SqliteLongTermMemory {
    connection: Connection,
    admission: Arc<Semaphore>,
    options: MemoryStoreOptions,
}
impl SqliteLongTermMemory {
    pub async fn open(
        path: impl AsRef<Path>,
        options: MemoryStoreOptions,
    ) -> Result<Self, MemoryError> {
        options.validate()?;
        let connection = Connection::open(path).await.map_err(database_error)?;
        connection.call(|connection| -> Result<(), MemoryError> {
            connection.busy_timeout(std::time::Duration::from_secs(5)).map_err(database_error)?;
            connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;").map_err(database_error)?;
            let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(database_error)?;
            tx.execute_batch("CREATE TABLE IF NOT EXISTS long_term_memory_meta (id INTEGER PRIMARY KEY CHECK(id=1), version INTEGER NOT NULL);
                INSERT OR IGNORE INTO long_term_memory_meta VALUES (1,1);
                CREATE TABLE IF NOT EXISTS long_term_memories (namespace TEXT NOT NULL, id TEXT NOT NULL, version TEXT NOT NULL, content TEXT NOT NULL, updated INTEGER NOT NULL, expires INTEGER, value TEXT NOT NULL, PRIMARY KEY(namespace,id));
                CREATE INDEX IF NOT EXISTS long_term_memory_lookup ON long_term_memories(namespace,updated DESC,id);").map_err(database_error)?;
            let version: u32 = tx.query_row("SELECT version FROM long_term_memory_meta WHERE id=1", [], |r| r.get(0)).map_err(database_error)?;
            if version != 1 { return Err(invalid("unsupported long-term memory format version")); }
            tx.commit().map_err(database_error)
        }).await.map_err(worker_error)?;
        Ok(Self {
            connection,
            admission: Arc::new(Semaphore::new(options.max_pending_operations)),
            options,
        })
    }
    pub async fn close(&self) -> Result<(), MemoryError> {
        self.admission.close();
        self.connection
            .clone()
            .close()
            .await
            .map_err(|_| backend("memory database close failed"))
    }
    async fn execute<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut rusqlite::Connection) -> Result<T, MemoryError> + Send + 'static,
    ) -> Result<T, MemoryError> {
        let permit = self
            .admission
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| backend("memory database closed"))?;
        self.connection
            .call(move |connection| {
                let _permit = permit;
                f(connection)
            })
            .await
            .map_err(worker_error)
    }
}
#[async_trait]
impl LongTermMemory for SqliteLongTermMemory {
    async fn get(&self, namespace: &str, id: &str) -> Result<MemoryEntry, MemoryError> {
        validate_key(namespace, id)?;
        let namespace = namespace.to_owned();
        let id = id.to_owned();
        let max = self.options.max_entry_bytes;
        self.execute(move |connection| {
            load_entry(connection, &namespace, &id, now_ms()?, max)?.ok_or(MemoryError::NotFound)
        })
        .await
    }
    async fn search(
        &self,
        namespace: &str,
        query: &MemoryQuery,
    ) -> Result<Vec<MemoryEntry>, MemoryError> {
        self.options.validate_query(namespace, query)?;
        let namespace = namespace.to_owned();
        let query = query.clone();
        let max = self.options.max_entry_bytes;
        self.execute(move |connection| {
            let mut sql = "SELECT length(CAST(value AS BLOB)), value FROM long_term_memories WHERE namespace=? AND (expires IS NULL OR expires>?)".to_owned();
            let mut values = vec![rusqlite::types::Value::Text(namespace), rusqlite::types::Value::Integer(now_ms()? as i64)];
            for term in query.text.split_whitespace() {
                sql.push_str(" AND lower(content) LIKE ? ESCAPE '\\'");
                let term = term.to_ascii_lowercase().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
                values.push(rusqlite::types::Value::Text(format!("%{term}%")));
            }
            sql.push_str(" ORDER BY updated DESC, id ASC LIMIT ?");
            values.push(rusqlite::types::Value::Integer(query.limit.min(i64::MAX as usize) as i64));
            let mut statement = connection.prepare(&sql).map_err(database_error)?;
            let mut rows = statement.query(rusqlite::params_from_iter(values)).map_err(database_error)?;
            let mut entries = Vec::new();
            let mut bytes: usize = 0;
            while let Some(row) = rows.next().map_err(database_error)? {
                let size: usize = row.get(0).map_err(database_error)?;
                if size > max { return Err(invalid("persisted memory exceeds configured size limit")); }
                if size > query.max_bytes.saturating_sub(bytes) { continue; }
                let encoded: String = row.get(1).map_err(database_error)?;
                let entry = decode_entry(&encoded)?;
                bytes += size;
                entries.push(entry);
            }
            Ok(entries)
        }).await
    }
    async fn save(
        &self,
        namespace: &str,
        value: &MemoryWrite,
        expected: Option<&str>,
    ) -> Result<MemoryEntry, MemoryError> {
        let entry = make_entry(namespace, value, now_ms()?, self.options.max_entry_bytes)?;
        let expected = expected.map(str::to_owned);
        let max = self.options.max_entry_bytes;
        let max_entries = self.options.max_entries;
        self.execute(move |connection| {
            let now = now_ms()?;
            if !active(&entry, now) { return Err(invalid("cannot save already expired memory")); }
            let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(database_error)?;
            tx.execute("DELETE FROM long_term_memories WHERE expires IS NOT NULL AND expires<=?", [now]).map_err(database_error)?;
            let previous = load_entry(&tx, &entry.namespace, &entry.value.id, now, max)?;
            if previous.as_ref().map(|e| &e.version) != expected.as_ref() { return Err(MemoryError::Conflict); }
            if previous.is_none() {
                let count: usize = tx.query_row("SELECT count(*) FROM long_term_memories", [], |r| r.get(0)).map_err(database_error)?;
                if count >= max_entries { return Err(MemoryError::Capacity); }
            }
            let value = encode_entry(&entry, max)?;
            tx.execute("INSERT INTO long_term_memories(namespace,id,version,content,updated,expires,value) VALUES (?1,?2,?3,?4,?5,?6,?7)
                ON CONFLICT(namespace,id) DO UPDATE SET version=excluded.version,content=excluded.content,updated=excluded.updated,expires=excluded.expires,value=excluded.value",
                params![entry.namespace,entry.value.id,entry.version,entry.value.content,entry.updated_at_ms,entry.value.expires_at_ms,value]).map_err(database_error)?;
            tx.commit().map_err(database_error)?;
            Ok(entry)
        }).await
    }
    async fn delete(&self, namespace: &str, id: &str, expected: &str) -> Result<(), MemoryError> {
        validate_key(namespace, id)?;
        let namespace = namespace.to_owned();
        let id = id.to_owned();
        let expected = expected.to_owned();
        let max = self.options.max_entry_bytes;
        self.execute(move |connection| {
            let tx = connection
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(database_error)?;
            let previous =
                load_entry(&tx, &namespace, &id, now_ms()?, max)?.ok_or(MemoryError::NotFound)?;
            if previous.version != expected {
                return Err(MemoryError::Conflict);
            }
            tx.execute(
                "DELETE FROM long_term_memories WHERE namespace=?1 AND id=?2",
                params![namespace, id],
            )
            .map_err(database_error)?;
            tx.commit().map_err(database_error)
        })
        .await
    }
}
fn load_entry(
    connection: &rusqlite::Connection,
    namespace: &str,
    id: &str,
    now: u64,
    max: usize,
) -> Result<Option<MemoryEntry>, MemoryError> {
    connection.query_row("SELECT length(CAST(value AS BLOB)), value FROM long_term_memories WHERE namespace=?1 AND id=?2 AND (expires IS NULL OR expires>?3)", params![namespace,id,now], |row| {
        let bytes: usize = row.get(0)?;
        if bytes > max { return Err(rusqlite::Error::InvalidQuery); }
        row.get::<_, String>(1)
    }).optional().map_err(database_error)?.map(|encoded| {
        let entry = decode_entry(&encoded)?;
        if entry.namespace != namespace || entry.value.id != id { return Err(invalid("persisted memory identity mismatch")); }
        Ok(entry)
    }).transpose()
}
fn make_entry(
    namespace: &str,
    value: &MemoryWrite,
    now: u64,
    max: usize,
) -> Result<MemoryEntry, MemoryError> {
    validate_key(namespace, &value.id)?;
    if value.content.trim().is_empty()
        || value.content.len() > max
        || value.source.id.trim().is_empty()
        || value.source.id.len() > 2048
        || value
            .source
            .version
            .as_ref()
            .is_some_and(|s| s.len() > 1024)
        || value
            .expires_at_ms
            .is_some_and(|time| time <= now || time > i64::MAX as u64)
    {
        return Err(invalid("invalid memory content, source or expiration"));
    }
    let entry = MemoryEntry {
        namespace: namespace.into(),
        version: uuid::Uuid::new_v4().to_string(),
        updated_at_ms: now,
        value: value.clone(),
    };
    encode_entry(&entry, max)?;
    Ok(entry)
}
fn encode_entry(entry: &MemoryEntry, max: usize) -> Result<String, MemoryError> {
    let encoded =
        serde_json::to_string(entry).map_err(|_| invalid("memory is not serializable"))?;
    if encoded.len() > max {
        return Err(MemoryError::Capacity);
    }
    Ok(encoded)
}
fn decode_entry(encoded: &str) -> Result<MemoryEntry, MemoryError> {
    let entry: MemoryEntry =
        serde_json::from_str(encoded).map_err(|_| invalid("persisted memory is corrupt"))?;
    validate_key(&entry.namespace, &entry.value.id)?;
    if entry.version.is_empty() {
        return Err(invalid("persisted memory version is missing"));
    }
    Ok(entry)
}
pub(crate) fn validate_namespace(namespace: &str) -> Result<(), MemoryError> {
    if namespace.trim().is_empty() || namespace.len() > 512 {
        return Err(invalid("namespace must contain 1..=512 bytes"));
    }
    Ok(())
}
fn validate_key(namespace: &str, id: &str) -> Result<(), MemoryError> {
    validate_namespace(namespace)?;
    if id.trim().is_empty() || id.len() > 512 {
        return Err(invalid("memory ID must contain 1..=512 bytes"));
    }
    Ok(())
}
pub(crate) fn now_ms() -> Result<u64, MemoryError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|time| time.as_millis().min(i64::MAX as u128) as u64)
        .map_err(|_| backend("system clock precedes Unix epoch"))
}
fn invalid(message: &str) -> MemoryError {
    MemoryError::Invalid(message.into())
}
fn backend(message: &str) -> MemoryError {
    MemoryError::Backend(message.into())
}
fn database_error(error: rusqlite::Error) -> MemoryError {
    match error {
        rusqlite::Error::SqliteFailure(failure, _) => {
            backend(&format!("SQLite error {:?}", failure.code))
        }
        _ => backend("SQLite operation failed"),
    }
}
fn worker_error(error: tokio_rusqlite::Error<MemoryError>) -> MemoryError {
    match error {
        tokio_rusqlite::Error::Error(error) => error,
        _ => backend("SQLite memory worker unavailable"),
    }
}

fn active(entry: &MemoryEntry, now: u64) -> bool {
    entry.value.expires_at_ms.is_none_or(|time| time > now)
}

#[async_trait]
impl eliteforge_agent_sqlite_extensions::SqliteExtensionHost for SqliteLongTermMemory {
    async fn prepare_extensions(
        &self,
        config: eliteforge_agent_sqlite_extensions::ExtensionConfig,
    ) -> Result<
        eliteforge_agent_sqlite_extensions::ExtensionReport,
        eliteforge_agent_sqlite_extensions::ExtensionError,
    > {
        self.execute(move |connection| Ok(config.prepare_connection(connection)))
            .await
            .map_err(|_| eliteforge_agent_sqlite_extensions::ExtensionError::Worker)?
    }
}
