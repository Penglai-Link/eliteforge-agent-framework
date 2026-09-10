//! Versioned, derived conversation summaries. Source transcripts remain authoritative.
use async_trait::async_trait;
use eliteforge_agent_core::{
    ContextError, ConversationKey, Plugin, PluginContext, PluginError, extension_point,
};
use serde::{Deserialize, Serialize};
use std::{path::Path, sync::Arc};
use tokio::sync::Semaphore;
use tokio_rusqlite::{
    Connection,
    rusqlite::{OptionalExtension, params},
};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SummaryKey {
    pub conversation: ConversationKey,
    pub generation: String,
    pub block_id: String,
    pub policy_version: String,
    /// Fingerprint of the algorithm, models, quotas and compression options.
    pub configuration_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationSummary {
    pub format_version: u32,
    /// The snapshot version from which this summary was produced.
    pub source_revision: u64,
    /// Covers sequences 1 through this value, including any preserved instructions.
    /// The digest includes every original field in that prefix.
    pub covered_messages: u64,
    pub source_digest: String,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VersionedSummary {
    /// Opaque CAS identity. Never reused, including after deletion.
    pub version: String,
    pub value: ConversationSummary,
}

#[extension_point(single, adapter = crate::SummaryStorePlugin)]
#[async_trait]
pub trait SummaryStore: Send + Sync {
    async fn load(&self, key: &SummaryKey) -> Result<Option<VersionedSummary>, ContextError>;
    /// None means create only if absent. False means a concurrent writer won;
    /// the caller may still use its valid local projection without overwriting it.
    async fn compare_exchange(
        &self,
        key: &SummaryKey,
        expected: Option<&str>,
        value: &ConversationSummary,
    ) -> Result<bool, ContextError>;
    async fn delete(&self, key: &SummaryKey, expected: &str) -> Result<bool, ContextError>;
}

/// The unique summary-store service belongs with the storage contract, not a
/// particular database implementation.
pub struct SummaryStoreService(pub Arc<dyn SummaryStore>);

pub struct SummaryStorePlugin(pub Arc<dyn SummaryStore>);

impl SummaryStorePlugin {
    pub fn new(store: impl SummaryStore + 'static) -> Self {
        Self(Arc::new(store))
    }

    pub fn from_arc(store: Arc<dyn SummaryStore>) -> Self {
        Self(store)
    }
}

#[async_trait]
impl Plugin for SummaryStorePlugin {
    fn id(&self) -> &str {
        "context.summary-store"
    }

    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        context.provide(SummaryStoreService(self.0.clone()))?;
        context.contribute_extension(self.0.clone())
    }
}

#[derive(Debug, Clone)]
pub struct SummaryStoreOptions {
    pub max_records: usize,
    pub max_record_bytes: usize,
    pub max_pending_operations: usize,
}
impl Default for SummaryStoreOptions {
    fn default() -> Self {
        Self {
            max_records: 10_000,
            max_record_bytes: 64 * 1024,
            max_pending_operations: 64,
        }
    }
}
impl SummaryStoreOptions {
    fn validate(&self) -> Result<(), ContextError> {
        if self.max_records == 0
            || self.max_record_bytes == 0
            || self.max_pending_operations == 0
            || self.max_pending_operations > Semaphore::MAX_PERMITS
        {
            return Err(error("summary store limits must be positive"));
        }
        Ok(())
    }
}

/// A local persistent cache with atomic compare-and-swap. SQLite is confined to
/// the plugin crate; the Framework has no database dependency.
#[derive(Clone)]
pub struct SqliteSummaryStore {
    connection: Connection,
    admission: Arc<Semaphore>,
    options: SummaryStoreOptions,
}
impl SqliteSummaryStore {
    pub async fn open(
        path: impl AsRef<Path>,
        options: SummaryStoreOptions,
    ) -> Result<Self, ContextError> {
        options.validate()?;
        let connection = Connection::open(path).await.map_err(sql_error)?;
        connection.call(|connection| -> Result<(), ContextError> {
            connection.busy_timeout(std::time::Duration::from_secs(5)).map_err(sql_error)?;
            connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;").map_err(sql_error)?;
            let transaction = connection.transaction_with_behavior(tokio_rusqlite::rusqlite::TransactionBehavior::Immediate).map_err(sql_error)?;
            transaction.execute_batch("CREATE TABLE IF NOT EXISTS context_summary_meta (id INTEGER PRIMARY KEY CHECK(id=1), version INTEGER NOT NULL);
                INSERT OR IGNORE INTO context_summary_meta VALUES (1, 1);
                CREATE TABLE IF NOT EXISTS context_summaries (key TEXT PRIMARY KEY, version TEXT NOT NULL, source_revision INTEGER NOT NULL, value TEXT NOT NULL);").map_err(sql_error)?;
            let version: u32 = transaction.query_row("SELECT version FROM context_summary_meta WHERE id=1", [], |row| row.get(0)).map_err(sql_error)?;
            if version != 1 { return Err(error("unsupported summary database version")); }
            transaction.commit().map_err(sql_error)
        }).await.map_err(worker_error)?;
        Ok(Self {
            connection,
            admission: Arc::new(Semaphore::new(options.max_pending_operations)),
            options,
        })
    }
    pub async fn close(&self) -> Result<(), ContextError> {
        self.admission.close();
        self.connection
            .clone()
            .close()
            .await
            .map_err(|_| error("summary database close failed"))
    }
    async fn execute<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut tokio_rusqlite::rusqlite::Connection) -> Result<T, ContextError>
        + Send
        + 'static,
    ) -> Result<T, ContextError> {
        let permit = self
            .admission
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| error("summary store is closed"))?;
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
impl SummaryStore for SqliteSummaryStore {
    async fn load(&self, key: &SummaryKey) -> Result<Option<VersionedSummary>, ContextError> {
        let key = encode_key(key)?;
        let max = self.options.max_record_bytes;
        let loaded = self.execute(move |connection| {
            connection.query_row("SELECT version, length(CAST(value AS BLOB)), value FROM context_summaries WHERE key=?1", [key], |row| {
                let length: usize = row.get(1)?;
                if length > max { return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery); }
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(2)?))
            }).optional().map_err(sql_error)
        }).await?;
        loaded
            .map(|(version, data)| {
                let value: ConversationSummary =
                    serde_json::from_str(&data).map_err(|_| error("invalid persisted summary"))?;
                encode_value(&value, max)?;
                Ok(VersionedSummary { version, value })
            })
            .transpose()
    }
    async fn compare_exchange(
        &self,
        key: &SummaryKey,
        expected: Option<&str>,
        value: &ConversationSummary,
    ) -> Result<bool, ContextError> {
        let key = encode_key(key)?;
        let data = encode_value(value, self.options.max_record_bytes)?;
        let source_revision = i64::try_from(value.source_revision)
            .map_err(|_| error("summary source revision overflow"))?;
        let expected = expected.map(str::to_owned);
        let max_records = self.options.max_records;
        self.execute(move |connection| {
            let tx = connection.transaction_with_behavior(tokio_rusqlite::rusqlite::TransactionBehavior::Immediate).map_err(sql_error)?;
            let current: Option<(String, i64)> = tx.query_row("SELECT version, source_revision FROM context_summaries WHERE key=?1", [&key], |row| Ok((row.get(0)?, row.get(1)?))).optional().map_err(sql_error)?;
            if current.as_ref().map(|v| &v.0) != expected.as_ref() || current.as_ref().is_some_and(|v| v.1 > source_revision) { return Ok(false); }
            if current.is_none() {
                let count: usize = tx.query_row("SELECT count(*) FROM context_summaries", [], |row| row.get(0)).map_err(sql_error)?;
                if count >= max_records { return Err(error("summary store record limit reached")); }
            }
            tx.execute("INSERT INTO context_summaries (key,version,source_revision,value) VALUES (?1,?2,?3,?4) ON CONFLICT(key) DO UPDATE SET version=excluded.version, source_revision=excluded.source_revision, value=excluded.value", params![key, uuid::Uuid::new_v4().to_string(), source_revision, data]).map_err(sql_error)?;
            tx.commit().map_err(sql_error)?;
            Ok(true)
        }).await
    }
    async fn delete(&self, key: &SummaryKey, expected: &str) -> Result<bool, ContextError> {
        let key = encode_key(key)?;
        let expected = expected.to_owned();
        self.execute(move |connection| {
            Ok(connection
                .execute(
                    "DELETE FROM context_summaries WHERE key=?1 AND version=?2",
                    params![key, expected],
                )
                .map_err(sql_error)?
                == 1)
        })
        .await
    }
}

fn error(message: &str) -> ContextError {
    ContextError::Other(message.into())
}
fn sql_error(_: tokio_rusqlite::rusqlite::Error) -> ContextError {
    // SQL errors may contain paths or data. Keep exported error text bounded and private.
    error("summary database operation failed")
}
fn worker_error(failure: tokio_rusqlite::Error<ContextError>) -> ContextError {
    match failure {
        tokio_rusqlite::Error::Error(error) => error,
        _ => error("summary database worker unavailable"),
    }
}
fn encode_key(key: &SummaryKey) -> Result<String, ContextError> {
    key.conversation.validate()?;
    if [&key.generation, &key.block_id, &key.policy_version]
        .iter()
        .any(|s| s.trim().is_empty() || s.len() > 1024)
    {
        return Err(error("invalid summary key"));
    }
    if key.configuration_digest.len() != 64
        || !key
            .configuration_digest
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
    {
        return Err(error("invalid summary configuration digest"));
    }
    let encoded = serde_json::to_string(key).map_err(|_| error("invalid summary key"))?;
    if encoded.len() > 4096 {
        return Err(error("summary key exceeds size limit"));
    }
    Ok(encoded)
}
fn encode_value(value: &ConversationSummary, max: usize) -> Result<String, ContextError> {
    if value.format_version != 1
        || value.covered_messages == 0
        || value.covered_messages > value.source_revision
        || value.source_digest.len() != 64
        || !value.source_digest.bytes().all(|b| b.is_ascii_hexdigit())
        || value.summary.trim().is_empty()
    {
        return Err(error("invalid summary record"));
    }
    if value.summary.len() > max {
        return Err(error("summary record exceeds size limit"));
    }
    let encoded = serde_json::to_string(value).map_err(|_| error("invalid summary record"))?;
    if encoded.len() > max {
        return Err(error("summary record exceeds size limit"));
    }
    Ok(encoded)
}

#[async_trait]
impl eliteforge_agent_sqlite_extensions::SqliteExtensionHost for SqliteSummaryStore {
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
