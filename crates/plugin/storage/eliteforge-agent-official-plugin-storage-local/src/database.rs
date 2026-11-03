//! One injected database runtime for framework stores and host repositories.
use async_trait::async_trait;
use eliteforge_agent_core::{StateError, TransactionManager};
use std::{
    future::Future,
    ops::Deref,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tokio::sync::{Mutex, OwnedMutexGuard};
use tokio_rusqlite::{Connection, rusqlite};

const READER_POOL_SIZE: usize = 4;

#[derive(Clone)]
pub struct SqliteDatabase(Arc<DatabaseInner>);
struct DatabaseInner {
    connection: Connection,
    gate: Arc<Mutex<()>>,
    readers: std::sync::RwLock<Vec<Connection>>,
    reader_next: AtomicUsize,
}
#[derive(Clone)]
struct Binding {
    database: SqliteDatabase,
    rollback_only: Arc<AtomicBool>,
    after_commit: Arc<std::sync::Mutex<Vec<futures::future::BoxFuture<'static, ()>>>>,
}
tokio::task_local! { static TRANSACTION: Binding; }

impl SqliteDatabase {
    pub async fn open(path: impl AsRef<Path>) -> rusqlite::Result<Self> {
        let path = path.as_ref();
        let connection = Connection::open(path).await?;
        connection
            .call(|c| -> rusqlite::Result<()> {
                c.busy_timeout(std::time::Duration::from_secs(5))?;
                // Pointer maps must be enabled before the first schema is
                // created. Existing databases keep their format: converting
                // those requires an explicit, separately scheduled VACUUM.
                if c.query_row("PRAGMA page_count", [], |row| row.get::<_, u64>(0))? == 0 {
                    c.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
                }
                // WAL keeps transactions durable across process crashes at
                // synchronous=NORMAL; only an OS-level power loss can lose the
                // latest commits. FULL would fsync every commit for no
                // practical gain in this embedded single-writer deployment.
                c.execute_batch(
                    "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;",
                )
            })
            .await
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let readers = open_readers(path).await;
        Ok(Self(Arc::new(DatabaseInner {
            connection,
            gate: Arc::new(Mutex::new(())),
            readers: std::sync::RwLock::new(readers),
            reader_next: AtomicUsize::new(0),
        })))
    }

    /// WAL allows one writer plus any number of readers. Read-only pool
    /// connections let host repository reads run concurrently instead of
    /// queueing behind the serialized write connection.
    pub async fn call_read<F, T, E>(&self, operation: F) -> Result<T, tokio_rusqlite::Error<E>>
    where
        F: FnOnce(&mut rusqlite::Connection) -> Result<T, E> + Send + 'static,
        T: Send + 'static,
        E: Send + 'static,
    {
        // A read inside an active transaction must observe its own writes and
        // keep the savepoint ordering, so it joins the write connection.
        if TRANSACTION.try_with(|_| ()).is_ok() {
            return self.call(operation).await;
        }
        let reader = self.0
            .readers
            .read()
            .ok()
            .and_then(|readers| {
                let count = readers.len();
                (count > 0).then(|| {
                    readers[self.0.reader_next.fetch_add(1, Ordering::Relaxed) % count].clone()
                })
            });
        match reader {
            Some(reader) => reader.call(operation).await,
            None => self.call(operation).await,
        }
    }

    /// Host repositories use this same handle, rather than opening connections.
    pub async fn call<F, T, E>(&self, operation: F) -> Result<T, tokio_rusqlite::Error<E>>
    where
        F: FnOnce(&mut rusqlite::Connection) -> Result<T, E> + Send + 'static,
        T: Send + 'static,
        E: Send + 'static,
    {
        let binding = TRANSACTION.try_with(Clone::clone).ok();
        if binding
            .as_ref()
            .is_some_and(|b| !Arc::ptr_eq(&b.database.0, &self.0))
        {
            if let Some(binding) = binding {
                binding.rollback_only.store(true, Ordering::Release);
            }
            return Err(tokio_rusqlite::Error::ConnectionClosed);
        }
        let permit = if binding.is_none() {
            Some(self.0.gate.clone().lock_owned().await)
        } else {
            None
        };
        self.0
            .connection
            .call(move |c| {
                let _permit = permit; // Queue owns the lock even when its awaiter drops.
                let result = operation(c);
                if result.is_err()
                    && let Some(binding) = binding
                {
                    binding.rollback_only.store(true, Ordering::Release);
                }
                result
            })
            .await
    }

    /// Synchronous host repositories execute on the same SQLite worker. The
    /// calling task's REQUIRED transaction binding is captured before blocking.
    /// Use from a blocking executor, or for short repository calls; never wait
    /// on unrelated application work while retaining a repository lock.
    pub fn call_blocking<F, T, E>(&self, operation: F) -> Result<T, tokio_rusqlite::Error<E>>
    where
        F: FnOnce(&mut rusqlite::Connection) -> Result<T, E> + Send + 'static,
        T: Send + 'static,
        E: Send + 'static,
    {
        Self::block_on_call(|| Box::pin(self.call(operation)))
    }

    /// Synchronous host read on the WAL reader pool. Join the write connection
    /// when a transaction is active on this thread.
    pub fn call_read_blocking<F, T, E>(&self, operation: F) -> Result<T, tokio_rusqlite::Error<E>>
    where
        F: FnOnce(&mut rusqlite::Connection) -> Result<T, E> + Send + 'static,
        T: Send + 'static,
        E: Send + 'static,
    {
        Self::block_on_call(|| Box::pin(self.call_read(operation)))
    }

    fn block_on_call<T, E, Fut>(
        call: impl FnOnce() -> Fut,
    ) -> Result<T, tokio_rusqlite::Error<E>>
    where
        Fut: Future<Output = Result<T, tokio_rusqlite::Error<E>>>,
    {
        // Synchronous callers cannot yield back to Tokio to replenish its task
        // budget. Keep the current task-local transaction while polling without
        // cooperative scheduling; the SQL still executes on the SQLite worker.
        let run = || futures::executor::block_on(tokio::task::unconstrained(call()));
        if tokio::runtime::Handle::try_current().is_ok_and(|handle| {
            handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread
        }) {
            // Free the runtime worker while waiting for a native transaction.
            // The closure stays on this thread and retains its task-local binding.
            tokio::task::block_in_place(run)
        } else {
            run()
        }
    }

    pub async fn transaction<T: Send>(
        &self,
        operation: impl Future<Output = Result<T, StateError>> + Send,
    ) -> Result<T, StateError> {
        if let Ok(binding) = TRANSACTION.try_with(Clone::clone) {
            if !Arc::ptr_eq(&binding.database.0, &self.0) {
                binding.rollback_only.store(true, Ordering::Release);
                return Err(StateError::Invalid(
                    "cannot join a transaction from another database runtime".into(),
                ));
            }
            let result = operation.await;
            if result.is_err() {
                binding.rollback_only.store(true, Ordering::Release);
            }
            return result;
        }
        let gate = self.0.gate.clone().lock_owned().await;
        let mut guard = TransactionGuard {
            database: self.clone(),
            gate: Some(gate),
        };
        self.0
            .connection
            .call(|c| -> rusqlite::Result<()> { c.execute_batch("BEGIN IMMEDIATE") })
            .await
            .map_err(worker)?;
        let binding = Binding {
            database: self.clone(),
            rollback_only: Arc::new(AtomicBool::new(false)),
            after_commit: Arc::default(),
        };
        let result = TRANSACTION.scope(binding.clone(), operation).await;
        let commit_requested = result.is_ok();
        let rollback_only = binding.rollback_only.clone();
        let commit = self
            .0
            .connection
            .call(move |c| -> rusqlite::Result<bool> {
                // Read after all previously queued SQL. A dropped SQL awaiter
                // can still fail and poison the transaction on the worker.
                let commit = commit_requested && !rollback_only.load(Ordering::Acquire);
                c.execute_batch(if commit { "COMMIT" } else { "ROLLBACK" })?;
                Ok(commit)
            })
            .await
            .map_err(worker)?;
        guard.gate.take();
        if commit {
            for callback in binding
                .after_commit
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .drain(..)
            {
                tokio::spawn(callback);
            }
        }
        if !commit && result.is_ok() {
            return Err(StateError::Conflict);
        }
        result
    }

    pub(crate) fn in_transaction(&self) -> bool {
        TRANSACTION.try_with(|_| ()).is_ok()
    }

    /// Best-effort derived work runs only after the outermost successful commit.
    /// Callbacks are discarded on rollback and survive cancellation of the caller.
    pub(crate) fn after_commit(&self, callback: impl Future<Output = ()> + Send + 'static) {
        let callback = Box::pin(callback);
        if let Ok(binding) = TRANSACTION.try_with(Clone::clone) {
            binding
                .after_commit
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(callback);
        } else {
            tokio::spawn(callback);
        }
    }

    /// Closing is owned by the injected runtime's host, never by individual stores.
    pub async fn close(self) -> Result<(), tokio_rusqlite::Error> {
        if self.in_transaction() {
            return Err(tokio_rusqlite::Error::ConnectionClosed);
        }
        let _gate = self.0.gate.clone().lock_owned().await;
        let readers = self
            .0
            .readers
            .write()
            .ok()
            .map(|mut readers| std::mem::take(&mut *readers))
            .unwrap_or_default();
        for reader in readers {
            let _ = reader.close().await;
        }
        self.0.connection.clone().close().await
    }
}

async fn open_readers(path: &Path) -> Vec<Connection> {
    let is_memory = path.as_os_str() == ":memory:" || path.to_string_lossy().starts_with("file::memory:");
    if is_memory {
        return Vec::new();
    }
    let mut readers = Vec::with_capacity(READER_POOL_SIZE);
    for _ in 0..READER_POOL_SIZE {
        let Ok(reader) = Connection::open(path).await else {
            break;
        };
        let configured = reader
            .call(|c| -> rusqlite::Result<()> {
                c.busy_timeout(std::time::Duration::from_secs(5))?;
                c.execute_batch("PRAGMA query_only=ON;")
            })
            .await
            .is_ok();
        if configured {
            readers.push(reader);
        }
    }
    readers
}

fn worker(_: impl std::fmt::Display) -> StateError {
    StateError::Backend("SQLite transaction failed".into())
}

struct TransactionGuard {
    database: SqliteDatabase,
    gate: Option<OwnedMutexGuard<()>>,
}
impl Drop for TransactionGuard {
    fn drop(&mut self) {
        if let Some(gate) = self.gate.take() {
            use futures::FutureExt;
            // Poll once to enqueue rollback immediately. The SQLite worker owns
            // the gate until rollback finishes, even when this task or runtime
            // has already stopped and cannot schedule a cleanup future.
            let _ = tokio::task::unconstrained(self.database.0.connection.call(
                move |c| -> rusqlite::Result<()> {
                    let _gate = gate;
                    if c.is_autocommit() {
                        Ok(())
                    } else {
                        c.execute_batch("ROLLBACK")
                    }
                },
            ))
            .now_or_never();
        }
    }
}

#[async_trait]
impl TransactionManager for SqliteDatabase {
    async fn transaction(
        &self,
        operation: futures::future::BoxFuture<'_, Result<(), StateError>>,
    ) -> Result<(), StateError> {
        SqliteDatabase::transaction(self, operation).await
    }
}

/// Store-local atomic operations join an existing connection transaction.
pub(crate) enum RequiredTransaction<'a> {
    Owned(rusqlite::Transaction<'a>),
    Joined(&'a mut rusqlite::Connection),
}
pub(crate) fn begin(c: &mut rusqlite::Connection) -> rusqlite::Result<RequiredTransaction<'_>> {
    if c.is_autocommit() {
        Ok(RequiredTransaction::Owned(c.transaction_with_behavior(
            rusqlite::TransactionBehavior::Immediate,
        )?))
    } else {
        Ok(RequiredTransaction::Joined(c))
    }
}
impl Deref for RequiredTransaction<'_> {
    type Target = rusqlite::Connection;
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Owned(tx) => tx,
            Self::Joined(c) => c,
        }
    }
}
impl RequiredTransaction<'_> {
    pub(crate) fn commit(self) -> rusqlite::Result<()> {
        match self {
            Self::Owned(tx) => tx.commit(),
            Self::Joined(_) => Ok(()),
        }
    }
}
