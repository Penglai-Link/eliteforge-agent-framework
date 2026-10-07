//! One injected database runtime for framework stores and host repositories.
use async_trait::async_trait;
use eliteforge_agent_core::{StateError, TransactionManager};
use std::{
    future::Future,
    ops::Deref,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::{Mutex, OwnedMutexGuard};
use tokio_rusqlite::{Connection, rusqlite};

#[derive(Clone)]
pub struct SqliteDatabase(Arc<DatabaseInner>);
struct DatabaseInner {
    connection: Connection,
    gate: Arc<Mutex<()>>,
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
        let connection = Connection::open(path).await?;
        connection
            .call(|c| -> rusqlite::Result<()> {
                c.busy_timeout(std::time::Duration::from_secs(5))?;
                c.execute_batch(
                    "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;",
                )
            })
            .await
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        Ok(Self(Arc::new(DatabaseInner {
            connection,
            gate: Arc::new(Mutex::new(())),
        })))
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
        self.0.connection.clone().close().await
    }
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
            let database = self.database.clone();
            // Rollback is queued behind any in-flight SQL, retaining exclusivity.
            tokio::spawn(async move {
                let _ = database
                    .0
                    .connection
                    .call(|c| -> rusqlite::Result<()> {
                        if c.is_autocommit() {
                            Ok(())
                        } else {
                            c.execute_batch("ROLLBACK")
                        }
                    })
                    .await;
                drop(gate);
            });
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
