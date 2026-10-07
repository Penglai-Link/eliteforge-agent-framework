//! Persistent append-only conversations with exclusive renewable leases. These
//! leases serialize history writers; workflow execution still uses StateStore.
use crate::SqliteDatabase as Connection;
use async_trait::async_trait;
use eliteforge_agent_core::*;
use std::{path::Path, sync::Arc, time::Duration};
use tokio::sync::Semaphore;
use tokio_rusqlite::rusqlite::{self, OptionalExtension, params};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct SqliteConversationOptions {
    pub max_sessions: usize,
    pub max_messages: usize,
    pub max_bytes: usize,
    /// Optional idle retention: zero disables expiry (the default).
    /// With a positive TTL, unused conversations expire on a later open.
    pub session_ttl: Duration,
    pub lease_duration: Duration,
    pub max_pending_operations: usize,
}
impl Default for SqliteConversationOptions {
    fn default() -> Self {
        Self {
            max_sessions: 1024,
            max_messages: 2048,
            max_bytes: 16 * 1024 * 1024,
            session_ttl: Duration::ZERO,
            lease_duration: Duration::from_secs(30),
            max_pending_operations: 64,
        }
    }
}
#[derive(Clone)]
pub struct SqliteConversationStore {
    connection: Connection,
    options: SqliteConversationOptions,
    admission: Arc<Semaphore>,
    shutdown: CancellationToken,
}
impl SqliteConversationStore {
    pub async fn open(
        path: impl AsRef<Path>,
        options: SqliteConversationOptions,
    ) -> Result<Self, ConversationError> {
        if [
            options.max_sessions,
            options.max_messages,
            options.max_bytes,
            options.max_pending_operations,
        ]
        .contains(&0)
            || options.max_pending_operations > Semaphore::MAX_PERMITS
            || options.lease_duration < Duration::from_millis(30)
            || options.lease_duration.as_millis() > i64::MAX as u128
            || options.session_ttl.as_millis() > i64::MAX as u128
        {
            return Err(invalid("invalid conversation limits or lease duration"));
        }
        let connection = Connection::open(path).await.map_err(db_error)?;
        Self::with_database(connection, options).await
    }
    pub async fn with_database(
        connection: Connection,
        options: SqliteConversationOptions,
    ) -> Result<Self, ConversationError> {
        if [
            options.max_sessions,
            options.max_messages,
            options.max_bytes,
            options.max_pending_operations,
        ]
        .contains(&0)
            || options.max_pending_operations > Semaphore::MAX_PERMITS
            || options.lease_duration < Duration::from_millis(30)
            || options.lease_duration.as_millis() > i64::MAX as u128
            || options.session_ttl.as_millis() > i64::MAX as u128
        {
            return Err(invalid("invalid conversation limits or lease duration"));
        }

        connection.call(|connection| -> Result<(), ConversationError> {
            connection.busy_timeout(Duration::from_secs(5)).map_err(db_error)?;
            connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;").map_err(db_error)?;
            let tx = crate::database::begin(connection).map_err(db_error)?;
            tx.execute_batch("CREATE TABLE IF NOT EXISTS ef_conversation_meta (id INTEGER PRIMARY KEY CHECK(id=1), version INTEGER NOT NULL);
                INSERT OR IGNORE INTO ef_conversation_meta VALUES (1,1);
                CREATE TABLE IF NOT EXISTS ef_conversations (session TEXT NOT NULL, branch TEXT NOT NULL, generation TEXT NOT NULL, revision INTEGER NOT NULL, messages TEXT NOT NULL, accessed INTEGER NOT NULL, owner TEXT, lease_until INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(session,branch));").map_err(db_error)?;
            crate::lifecycle::initialize(&tx).map_err(|e|invalid(&e.to_string()))?;
            let version: u32 = tx.query_row("SELECT version FROM ef_conversation_meta WHERE id=1", [], |r| r.get(0)).map_err(db_error)?;
            if version != 1 { return Err(invalid("unsupported conversation format version")); }
            tx.commit().map_err(db_error)
        }).await.map_err(worker_error)?;
        Ok(Self {
            connection,
            admission: Arc::new(Semaphore::new(options.max_pending_operations)),
            options,
            shutdown: CancellationToken::new(),
        })
    }
    /// The host closes active Agents/sessions before closing their shared store.
    pub async fn close(&self) -> Result<(), ConversationError> {
        self.shutdown.cancel();
        self.admission.close();
        Ok(())
    }
    async fn execute<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut rusqlite::Connection) -> Result<T, ConversationError> + Send + 'static,
    ) -> Result<T, ConversationError> {
        let permit = self
            .admission
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| invalid("conversation database closed"))?;
        self.connection
            .call(move |connection| {
                let _permit = permit;
                f(connection)
            })
            .await
            .map_err(worker_error)
    }
    async fn release(&self, key: ConversationKey, owner: String) -> Result<(), ConversationError> {
        self.execute(move |connection| {
            connection.execute("UPDATE ef_conversations SET owner=NULL, lease_until=0, accessed=?3 WHERE session=?1 AND branch=?2 AND owner=?4", params![key.session_id,key.branch,now_ms()?,owner]).map_err(db_error)?;
            Ok(())
        }).await
    }
}
#[async_trait]
impl ConversationStore for SqliteConversationStore {
    async fn history(
        &self,
        session_id: &str,
    ) -> Result<Vec<ConversationSnapshot>, ConversationError> {
        let session = session_id.to_owned();
        let options = self.options.clone();
        self.execute(move |connection| {
            let mut query = connection.prepare("SELECT branch,generation,revision,length(CAST(messages AS BLOB)),messages FROM ef_conversations WHERE session=?1 ORDER BY branch").map_err(db_error)?;
            let rows = query.query_map([&session], |row| {
                let revision:u64=row.get(2)?; let size:usize=row.get(3)?;
                if revision>options.max_messages as u64 || size>options.max_bytes { return Err(rusqlite::Error::InvalidQuery); }
                Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,revision,row.get::<_,String>(4)?))
            }).map_err(db_error)?.collect::<rusqlite::Result<Vec<_>>>().map_err(db_error)?;
            rows.into_iter().map(|(branch,generation,revision,data)| {
                let messages:Vec<Message>=serde_json::from_str(&data).map_err(|_|invalid("persisted conversation is corrupt"))?;
                if generation.is_empty() || messages.len() as u64!=revision {return Err(invalid("persisted conversation version mismatch"));}
                validate_messages(&messages)?;
                Ok(ConversationSnapshot::new(ConversationKey::new(&session).branch(branch),generation,messages))
            }).collect()
        }).await
    }

    async fn open(
        &self,
        session_id: &str,
    ) -> Result<Box<dyn ConversationSession>, ConversationError> {
        self.open_branch(&ConversationKey::new(session_id)).await
    }
    async fn open_branch(
        &self,
        key: &ConversationKey,
    ) -> Result<Box<dyn ConversationSession>, ConversationError> {
        key.validate()?;
        if key.session_id.len() > 512 || key.branch.len() > 2048 {
            return Err(invalid("conversation identity exceeds size limit"));
        }
        let key = key.clone();
        let owner = uuid::Uuid::new_v4().to_string();
        let options = self.options.clone();
        let key_for_open = key.clone();
        let owner_for_open = owner.clone();
        self.execute(move |connection| {
            let now = now_ms()?;
            let tx = crate::database::begin(connection).map_err(db_error)?;
            crate::lifecycle::ensure_session(&tx,&key_for_open.session_id).map_err(|e|invalid(&e.to_string()))?;
            if !options.session_ttl.is_zero() {
                tx.execute("DELETE FROM ef_conversations WHERE lease_until<=?1 AND accessed<=?2", params![now,now.saturating_sub(options.session_ttl.as_millis() as i64)]).map_err(db_error)?;
            }
            let previous: Option<(Option<String>, i64)> = tx.query_row("SELECT owner,lease_until FROM ef_conversations WHERE session=?1 AND branch=?2", params![key_for_open.session_id,key_for_open.branch], |r| Ok((r.get(0)?,r.get(1)?))).optional().map_err(db_error)?;
            if previous.as_ref().is_some_and(|(owner, expiry)| owner.is_some() && *expiry > now) { return Err(ConversationError::Conflict); }
            if previous.is_none() {
                let count: usize = tx.query_row("SELECT count(*) FROM ef_conversations", [], |r| r.get(0)).map_err(db_error)?;
                if count >= options.max_sessions { return Err(ConversationError::BudgetExceeded("persistent conversation count limit reached; expire or remove unused conversations".into())); }
                tx.execute("INSERT INTO ef_conversations(session,branch,generation,revision,messages,accessed) VALUES (?1,?2,?3,0,'[]',?4)", params![key_for_open.session_id,key_for_open.branch,uuid::Uuid::new_v4().to_string(),now]).map_err(db_error)?;
            }
            tx.execute("UPDATE ef_conversations SET owner=?3,lease_until=?4,accessed=?5 WHERE session=?1 AND branch=?2", params![key_for_open.session_id,key_for_open.branch,owner_for_open,now.saturating_add(options.lease_duration.as_millis() as i64),now]).map_err(db_error)?;
            tx.commit().map_err(db_error)
        }).await?;
        let cancellation = self.shutdown.child_token();
        let session = SqliteConversationSession {
            store: self.clone(),
            key: key.clone(),
            owner: owner.clone(),
            cancellation: cancellation.clone(),
            finished: false,
        };
        let store = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(store.options.lease_duration / 3);
            interval.tick().await;
            loop {
                tokio::select! { biased; _ = cancellation.cancelled() => break, _ = interval.tick() => {} }
                let key = key.clone();
                let owner = owner.clone();
                let duration = store.options.lease_duration.as_millis() as i64;
                let renewed = store.execute(move |connection| {
                    let now = now_ms()?;
                    // Renewal changes only expiry, keeping the fencing owner stable.
                    Ok(connection.execute("UPDATE ef_conversations SET lease_until=?4,accessed=?5 WHERE session=?1 AND branch=?2 AND owner=?3 AND lease_until>?5", params![key.session_id,key.branch,owner,now.saturating_add(duration),now]).map_err(db_error)? == 1)
                }).await;
                if !matches!(renewed, Ok(true)) {
                    cancellation.cancel();
                    break;
                }
            }
        });
        Ok(Box::new(session))
    }
}
struct SqliteConversationSession {
    store: SqliteConversationStore,
    key: ConversationKey,
    owner: String,
    cancellation: CancellationToken,
    finished: bool,
}
impl Drop for SqliteConversationSession {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if !self.finished
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let store = self.store.clone();
            let key = self.key.clone();
            let owner = self.owner.clone();
            runtime.spawn(async move {
                let _ = store.release(key, owner).await;
            });
        }
        // On hard process loss the lease expires; a stale owner can never append.
    }
}
impl SqliteConversationSession {
    fn ensure_active(&self) -> Result<(), ConversationError> {
        if self.finished || self.cancellation.is_cancelled() {
            Err(ConversationError::Conflict)
        } else {
            Ok(())
        }
    }
    async fn append_checked(
        &mut self,
        expected: Option<u64>,
        messages: &[Message],
    ) -> Result<(), ConversationError> {
        self.ensure_active()?;
        if messages.len() > self.store.options.max_messages {
            return Err(ConversationError::BudgetExceeded(
                "message count limit exceeded".into(),
            ));
        }
        let messages = messages.to_vec();
        let key = self.key.clone();
        let owner = self.owner.clone();
        let options = self.store.options.clone();
        self.store.execute(move |connection| {
            let tx = crate::database::begin(connection).map_err(db_error)?;
            let mut snapshot = load_snapshot(&tx, &key, &owner, &options)?;
            if expected.is_some_and(|revision| revision != snapshot.revision) { return Err(ConversationError::Conflict); }
            if snapshot.messages.len().saturating_add(messages.len()) > options.max_messages { return Err(ConversationError::BudgetExceeded("message count limit exceeded".into())); }
            snapshot.messages.extend(messages);
            validate_messages(&snapshot.messages)?;
            let encoded = encode_messages(&snapshot.messages, options.max_bytes)?;
            let now = now_ms()?;
            let changed = tx.execute("UPDATE ef_conversations SET messages=?4,revision=?5,accessed=?6 WHERE session=?1 AND branch=?2 AND owner=?3 AND lease_until>?6", params![key.session_id,key.branch,owner,encoded,snapshot.messages.len() as u64,now]).map_err(db_error)?;
            if changed != 1 { return Err(ConversationError::Conflict); }
            tx.commit().map_err(db_error)
        }).await
    }
}
#[async_trait]
impl ConversationSession for SqliteConversationSession {
    async fn snapshot(&mut self) -> Result<ConversationSnapshot, ConversationError> {
        self.ensure_active()?;
        let key = self.key.clone();
        let owner = self.owner.clone();
        let options = self.store.options.clone();
        self.store
            .execute(move |connection| load_snapshot(connection, &key, &owner, &options))
            .await
    }
    async fn append(&mut self, messages: &[Message]) -> Result<(), ConversationError> {
        self.append_checked(None, messages).await
    }
    async fn append_at(
        &mut self,
        revision: u64,
        messages: &[Message],
    ) -> Result<(), ConversationError> {
        self.append_checked(Some(revision), messages).await
    }
    async fn finish(&mut self, _: ConversationOutcome) -> Result<(), ConversationError> {
        self.cancellation.cancel();
        self.store
            .release(self.key.clone(), self.owner.clone())
            .await?;
        self.finished = true;
        Ok(())
    }
}
fn load_snapshot(
    connection: &rusqlite::Connection,
    key: &ConversationKey,
    owner: &str,
    options: &SqliteConversationOptions,
) -> Result<ConversationSnapshot, ConversationError> {
    let row: Option<(String, u64, String)> = connection.query_row("SELECT generation,revision,length(CAST(messages AS BLOB)),messages FROM ef_conversations WHERE session=?1 AND branch=?2 AND owner=?3 AND lease_until>?4", params![key.session_id,key.branch,owner,now_ms()?], |row| {
        let size: usize = row.get(2)?;
        let count: u64 = row.get(1)?;
        if size > options.max_bytes || count > options.max_messages as u64 { return Err(rusqlite::Error::InvalidQuery); }
        Ok((row.get(0)?,count,row.get(3)?))
    }).optional().map_err(db_error)?;
    let (generation, revision, data) = row.ok_or(ConversationError::Conflict)?;
    let messages: Vec<Message> =
        serde_json::from_str(&data).map_err(|_| invalid("persisted conversation is corrupt"))?;
    if generation.is_empty() || messages.len() as u64 != revision {
        return Err(invalid("persisted conversation version mismatch"));
    }
    validate_messages(&messages)?;
    Ok(ConversationSnapshot::new(key.clone(), generation, messages))
}
fn encode_messages(messages: &[Message], limit: usize) -> Result<String, ConversationError> {
    struct Bounded {
        bytes: Vec<u8>,
        max: usize,
    }
    impl std::io::Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.max.saturating_sub(self.bytes.len()) {
                return Err(std::io::Error::other("conversation too large"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Bounded {
        bytes: Vec::new(),
        max: limit,
    };
    serde_json::to_writer(&mut writer, messages).map_err(|_| {
        ConversationError::BudgetExceeded("conversation byte limit exceeded".into())
    })?;
    String::from_utf8(writer.bytes).map_err(|_| invalid("conversation encoding failed"))
}
fn now_ms() -> Result<i64, ConversationError> {
    crate::long_term::now_ms()
        .map(|time| time as i64)
        .map_err(|_| invalid("invalid conversation clock"))
}
fn invalid(message: &str) -> ConversationError {
    ConversationError::Other(message.into())
}
fn db_error(error: rusqlite::Error) -> ConversationError {
    match error {
        rusqlite::Error::SqliteFailure(failure, _) => {
            invalid(&format!("SQLite error {:?}", failure.code))
        }
        _ => invalid("SQLite operation failed"),
    }
}
fn worker_error(error: tokio_rusqlite::Error<ConversationError>) -> ConversationError {
    match error {
        tokio_rusqlite::Error::Error(error) => error,
        _ => invalid("SQLite conversation worker unavailable"),
    }
}

#[async_trait]
impl crate::extensions::SqliteExtensionHost for SqliteConversationStore {
    async fn prepare_extensions(
        &self,
        config: crate::extensions::ExtensionConfig,
    ) -> Result<crate::extensions::ExtensionReport, crate::extensions::ExtensionError> {
        self.execute(move |connection| Ok(config.prepare_connection(connection)))
            .await
            .map_err(|_| crate::extensions::ExtensionError::Worker)?
    }
}
