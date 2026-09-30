mod interactions;
pub use interactions::SqliteExecutionPlugin;
// Atomic checkpoint + transcript + event persistence. SQLite owns the clock
// used for leases. Multiple store instances/processes compete through SQL.
use async_trait::async_trait;
use eliteforge_agent_core::*;
use std::{path::Path, sync::Arc, time::Duration};
use tokio::sync::Semaphore;
use tokio_rusqlite::{
    Connection,
    rusqlite::{self, OptionalExtension, TransactionBehavior, params},
};

#[derive(Clone)]
pub struct SqliteStateStore {
    connection: Connection,
    admission: Arc<Semaphore>,
    max_checkpoint_bytes: usize,
}

#[derive(Debug, Clone)]
pub struct SqliteOptions {
    pub busy_timeout: Duration,
    pub max_pending_operations: usize,
    pub max_checkpoint_bytes: usize,
}
impl Default for SqliteOptions {
    fn default() -> Self {
        Self {
            busy_timeout: Duration::from_secs(5),
            max_pending_operations: 64,
            max_checkpoint_bytes: 32 * 1024 * 1024,
        }
    }
}

impl SqliteStateStore {
    pub async fn open(path: impl AsRef<Path>, options: SqliteOptions) -> Result<Self, StateError> {
        if options.busy_timeout.is_zero()
            || options.max_pending_operations == 0
            || options.max_pending_operations > Semaphore::MAX_PERMITS
            || options.max_checkpoint_bytes == 0
        {
            return Err(StateError::Invalid("SQLite limits must be positive".into()));
        }
        let connection = Connection::open(path).await.map_err(database_error)?;
        let timeout = options.busy_timeout;
        connection.call(move |connection| -> Result<(), StateError> {
            connection.busy_timeout(timeout).map_err(database_error)?;
            connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;").map_err(database_error)?;
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(database_error)?;
            tx.execute_batch("CREATE TABLE IF NOT EXISTS agent_core_metadata (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
                INSERT OR IGNORE INTO agent_core_metadata VALUES ('schema_version', 1);
                CREATE TABLE IF NOT EXISTS agent_checkpoints (
                    run_id TEXT PRIMARY KEY, session_id TEXT NOT NULL, revision INTEGER NOT NULL,
                    checkpoint TEXT NOT NULL, owner TEXT, generation INTEGER NOT NULL DEFAULT 0, expires INTEGER NOT NULL DEFAULT 0);
                CREATE TABLE IF NOT EXISTS agent_session_heads (session_id TEXT PRIMARY KEY, run_id TEXT NOT NULL REFERENCES agent_checkpoints(run_id));
                CREATE INDEX IF NOT EXISTS agent_checkpoint_sessions ON agent_checkpoints(session_id);
                CREATE TABLE IF NOT EXISTS agent_interactions (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, run_id TEXT NOT NULL, answered INTEGER NOT NULL, record TEXT NOT NULL);
                CREATE INDEX IF NOT EXISTS agent_interactions_pending ON agent_interactions(session_id,answered);").map_err(database_error)?;
            let version: i64 = tx.query_row("SELECT value FROM agent_core_metadata WHERE key='schema_version'", [], |row| row.get(0)).map_err(database_error)?;
            if version != 1 { return Err(StateError::UnsupportedVersion); }
            tx.commit().map_err(database_error)
        }).await.map_err(worker_error)?;
        Ok(Self {
            connection,
            admission: Arc::new(Semaphore::new(options.max_pending_operations)),
            max_checkpoint_bytes: options.max_checkpoint_bytes,
        })
    }

    pub fn plugin(self: Arc<Self>) -> SqliteExecutionPlugin {
        SqliteExecutionPlugin(self)
    }

    pub async fn close(&self) -> Result<(), StateError> {
        self.admission.close();
        self.connection
            .clone()
            .close()
            .await
            .map_err(|_| StateError::Backend("SQLite close failed".into()))
    }

    async fn execute<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut rusqlite::Connection) -> Result<T, StateError> + Send + 'static,
    ) -> Result<T, StateError> {
        let permit = self
            .admission
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| StateError::Backend("SQLite store is closed".into()))?;
        self.connection
            .call(move |connection| {
                // The permit belongs to queued SQL, not its cancellable awaiter.
                let _permit = permit;
                operation(connection)
            })
            .await
            .map_err(worker_error)
    }
}

fn database_error(error: rusqlite::Error) -> StateError {
    match error {
        rusqlite::Error::SqliteFailure(failure, _) => {
            StateError::Backend(format!("SQLite error {:?}", failure.code))
        }
        _ => StateError::Backend("SQLite operation failed".into()),
    }
}
fn worker_error(error: tokio_rusqlite::Error<StateError>) -> StateError {
    match error {
        tokio_rusqlite::Error::Error(error) => error,
        _ => StateError::Backend("SQLite worker unavailable".into()),
    }
}
fn encode(checkpoint: &Checkpoint, max_bytes: usize) -> Result<String, StateError> {
    let encoded = serde_json::to_string(checkpoint)
        .map_err(|_| StateError::Invalid("checkpoint cannot be serialized".into()))?;
    if encoded.len() > max_bytes {
        return Err(StateError::Invalid(
            "checkpoint exceeds storage size limit".into(),
        ));
    }
    Ok(encoded)
}
fn load(
    connection: &rusqlite::Connection,
    run_id: RunId,
    max_bytes: usize,
) -> Result<Checkpoint, StateError> {
    let (revision, encoded): (u64, String) = connection
        .query_row(
            "SELECT revision, checkpoint FROM agent_checkpoints WHERE run_id=?1",
            [run_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(database_error)?
        .ok_or(StateError::NotFound)?;
    if encoded.len() > max_bytes {
        return Err(StateError::Invalid(
            "stored checkpoint exceeds size limit".into(),
        ));
    }
    let checkpoint: Checkpoint = serde_json::from_str(&encoded)
        .map_err(|_| StateError::Invalid("stored checkpoint is corrupt".into()))?;
    checkpoint.validate()?;
    if checkpoint.run_id != run_id || checkpoint.revision != revision {
        return Err(StateError::Invalid(
            "stored checkpoint identity/revision mismatch".into(),
        ));
    }
    Ok(checkpoint)
}
fn db_now(connection: &rusqlite::Connection) -> Result<u64, StateError> {
    connection
        .query_row(
            "SELECT CAST(unixepoch('subsec') * 1000 AS INTEGER)",
            [],
            |row| row.get(0),
        )
        .map_err(database_error)
}
fn expiry(now: u64, ttl: Duration) -> Result<u64, StateError> {
    let ttl = u64::try_from(ttl.as_millis())
        .map_err(|_| StateError::Invalid("lease TTL out of range".into()))?;
    if ttl == 0 {
        return Err(StateError::Invalid(
            "lease TTL must be at least one millisecond".into(),
        ));
    }
    now.checked_add(ttl)
        .filter(|v| *v <= i64::MAX as u64)
        .ok_or_else(|| StateError::Invalid("lease expiry out of range".into()))
}
fn verify_lease(
    connection: &rusqlite::Connection,
    lease: &ExecutionLease,
) -> Result<(), StateError> {
    let (owner, generation, expires): (Option<String>, u64, u64) = connection
        .query_row(
            "SELECT owner, generation, expires FROM agent_checkpoints WHERE run_id=?1",
            [lease.run_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(database_error)?
        .ok_or(StateError::NotFound)?;
    if owner.as_deref() != Some(&lease.owner.to_string())
        || generation != lease.generation
        || expires <= db_now(connection)?
    {
        return Err(StateError::LeaseLost);
    }
    Ok(())
}

#[async_trait]
impl StateStore for SqliteStateStore {
    async fn create(
        &self,
        checkpoint: Checkpoint,
        previous_run: Option<RunId>,
    ) -> Result<Checkpoint, StateError> {
        checkpoint.validate()?;
        if checkpoint.revision != 0 || checkpoint.status != ExecutionStatus::Running {
            return Err(StateError::Invalid(
                "new checkpoint must start running at revision zero".into(),
            ));
        }
        let encoded = encode(&checkpoint, self.max_checkpoint_bytes)?;
        let max_bytes = self.max_checkpoint_bytes;
        self.execute(move |connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(database_error)?;
            let head: Option<String> = tx.query_row("SELECT run_id FROM agent_session_heads WHERE session_id=?1", [&checkpoint.session_id], |row| row.get(0)).optional().map_err(database_error)?;
            if head != previous_run.map(|id| id.to_string()) { return Err(StateError::Conflict); }
            if let Some(previous) = previous_run {
                let previous = load(&tx, previous, max_bytes)?;
                if !previous.status.is_terminal() { return Err(StateError::Busy); }
                if !checkpoint.transcript.starts_with(&previous.transcript) { return Err(StateError::Invalid("new run must retain previous transcript".into())); }
            }
            let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM agent_checkpoints WHERE run_id=?1)", [checkpoint.run_id.to_string()], |row| row.get(0)).map_err(database_error)?;
            if exists { return Err(StateError::Conflict); }
            tx.execute("INSERT INTO agent_checkpoints(run_id, session_id, revision, checkpoint) VALUES (?1,?2,0,?3)", params![checkpoint.run_id.to_string(), checkpoint.session_id, encoded]).map_err(database_error)?;
            tx.execute("INSERT INTO agent_session_heads(session_id,run_id) VALUES (?1,?2) ON CONFLICT(session_id) DO UPDATE SET run_id=excluded.run_id", params![checkpoint.session_id, checkpoint.run_id.to_string()]).map_err(database_error)?;
            tx.commit().map_err(database_error)?;
            Ok(checkpoint)
        }).await
    }
    async fn load(&self, run_id: RunId) -> Result<Checkpoint, StateError> {
        let max_bytes = self.max_checkpoint_bytes;
        self.execute(move |connection| load(connection, run_id, max_bytes))
            .await
    }
    async fn latest(&self, session_id: &str) -> Result<Option<Checkpoint>, StateError> {
        let session_id = session_id.to_owned();
        let max_bytes = self.max_checkpoint_bytes;
        self.execute(move |connection| {
            let id: Option<String> = connection
                .query_row(
                    "SELECT run_id FROM agent_session_heads WHERE session_id=?1",
                    [session_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(database_error)?;
            id.map(|id| {
                load(
                    connection,
                    id.parse()
                        .map_err(|_| StateError::Invalid("invalid stored run ID".into()))?,
                    max_bytes,
                )
            })
            .transpose()
        })
        .await
    }
    async fn claim(
        &self,
        run_id: RunId,
        owner: RunId,
        ttl: Duration,
    ) -> Result<ExecutionLease, StateError> {
        if owner.is_nil() {
            return Err(StateError::Invalid("lease owner cannot be nil".into()));
        }
        let max_bytes = self.max_checkpoint_bytes;
        self.execute(move |connection| {
            let tx = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(database_error)?;
            if load(&tx, run_id, max_bytes)?.status.is_terminal() {
                return Err(StateError::Conflict);
            }
            let now = db_now(&tx)?;
            let expires_at_unix_ms = expiry(now, ttl)?;
            let (current_owner, generation, expires): (Option<String>, u64, u64) = tx
                .query_row(
                    "SELECT owner,generation,expires FROM agent_checkpoints WHERE run_id=?1",
                    [run_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .map_err(database_error)?;
            if current_owner.is_some() && expires > now {
                return Err(StateError::Busy);
            }
            let generation = generation
                .checked_add(1)
                .filter(|g| *g <= i64::MAX as u64)
                .ok_or(StateError::Conflict)?;
            tx.execute(
                "UPDATE agent_checkpoints SET owner=?2,generation=?3,expires=?4 WHERE run_id=?1",
                params![
                    run_id.to_string(),
                    owner.to_string(),
                    generation,
                    expires_at_unix_ms
                ],
            )
            .map_err(database_error)?;
            tx.commit().map_err(database_error)?;
            Ok(ExecutionLease {
                run_id,
                owner,
                generation,
                expires_at_unix_ms,
            })
        })
        .await
    }
    async fn renew(
        &self,
        lease: &ExecutionLease,
        ttl: Duration,
    ) -> Result<ExecutionLease, StateError> {
        let lease = lease.clone();
        self.execute(move |connection| {
            let tx = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(database_error)?;
            verify_lease(&tx, &lease)?;
            let expires_at_unix_ms = expiry(db_now(&tx)?, ttl)?;
            tx.execute(
                "UPDATE agent_checkpoints SET expires=?2 WHERE run_id=?1",
                params![lease.run_id.to_string(), expires_at_unix_ms],
            )
            .map_err(database_error)?;
            tx.commit().map_err(database_error)?;
            Ok(ExecutionLease {
                expires_at_unix_ms,
                ..lease
            })
        })
        .await
    }
    async fn commit(
        &self,
        lease: &ExecutionLease,
        expected_revision: u64,
        mut next: Checkpoint,
    ) -> Result<Checkpoint, StateError> {
        let lease = lease.clone();
        let max_bytes = self.max_checkpoint_bytes;
        self.execute(move |connection| {
            let tx = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(database_error)?;
            verify_lease(&tx, &lease)?;
            let previous = load(&tx, lease.run_id, max_bytes)?;
            if previous.revision != expected_revision || next.revision != expected_revision {
                return Err(StateError::Conflict);
            }
            previous.validate_successor(&next)?;
            next.revision = expected_revision
                .checked_add(1)
                .filter(|v| *v <= i64::MAX as u64)
                .ok_or(StateError::Conflict)?;
            let encoded = encode(&next, max_bytes)?;
            verify_lease(&tx, &lease)?;
            tx.execute(
                "UPDATE agent_checkpoints SET revision=?2,checkpoint=?3 WHERE run_id=?1",
                params![lease.run_id.to_string(), next.revision, encoded],
            )
            .map_err(database_error)?;
            tx.commit().map_err(database_error)?;
            Ok(next)
        })
        .await
    }
    async fn release(&self, lease: &ExecutionLease) -> Result<(), StateError> {
        let lease = lease.clone();
        self.execute(move |connection| {
            let count = connection.execute("UPDATE agent_checkpoints SET owner=NULL,expires=0 WHERE run_id=?1 AND owner=?2 AND generation=?3",params![lease.run_id.to_string(),lease.owner.to_string(),lease.generation]).map_err(database_error)?;
            if count == 0 { return Err(StateError::LeaseLost); } Ok(())
        }).await
    }
}

#[async_trait]
impl crate::extensions::SqliteExtensionHost for SqliteStateStore {
    async fn prepare_extensions(
        &self,
        config: crate::extensions::ExtensionConfig,
    ) -> Result<
        crate::extensions::ExtensionReport,
        crate::extensions::ExtensionError,
    > {
        self.execute(move |connection| Ok(config.prepare_connection(connection)))
            .await
            .map_err(|_| crate::extensions::ExtensionError::Worker)?
    }
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_write_keeps_admission_until_sql_finishes_and_may_commit() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("unknown.db");
        let store = SqliteStateStore::open(
            &path,
            SqliteOptions {
                max_pending_operations: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let blocker = Connection::open(&path).await.unwrap();
        blocker
            .call(|connection| -> Result<(), StateError> {
                connection
                    .execute_batch("BEGIN IMMEDIATE")
                    .map_err(database_error)
            })
            .await
            .unwrap();
        let checkpoint = Checkpoint::new(RunId::new_v4(), "s", "test", 1, serde_json::json!({}));
        let id = checkpoint.run_id;
        let writer = store.clone();
        let task = tokio::spawn(async move { writer.create(checkpoint, None).await });
        // The current-thread runtime cannot observe this permit until the writer
        // has yielded waiting for the worker's SQL, which is blocked by SQLite.
        tokio::time::timeout(Duration::from_secs(2), async {
            while store.admission.available_permits() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(store.admission.available_permits(), 0);
        blocker
            .call(|connection| -> Result<(), StateError> {
                connection.execute_batch("ROLLBACK").map_err(database_error)
            })
            .await
            .unwrap();
        let loaded = tokio::time::timeout(Duration::from_secs(2), store.load(id))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.run_id, id);
        assert_eq!(loaded.revision, 0);
        assert_eq!(store.admission.available_permits(), 1);
        store.close().await.unwrap();
        blocker.close().await.unwrap();
    }
}
