//! Administrative deletion of persisted session data.
use crate::{Plugin, PluginContext, PluginError, StateError};
use async_trait::async_trait;
use std::{collections::HashSet, sync::Arc};

#[derive(Debug)]
pub struct SessionDeletionFailure {
    pub session_id: String,
    pub error: StateError,
}

#[derive(Debug, Default)]
pub struct SessionDeletionReport {
    /// Number of distinct sessions with persisted data removed.
    pub deleted: usize,
    /// Failures in input order. Missing sessions are successful no-ops.
    pub failures: Vec<SessionDeletionFailure>,
}

/// Session-scoped storage cleanup, independent of execution and history writers.
/// The host must stop runs and background jobs for these identities before
/// deleting them, and authorize access to the selected store and session IDs.
#[crate::extension_point(single, adapter = crate::SessionStorePlugin)]
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Delete all persisted data owned by this session. Missing sessions return
    /// false. Shared memory and catalogs retain their own lifecycle; owned artifacts
    /// are cleaned when their final session reference is removed.
    /// Cleanup for this identity is atomic. Blank IDs are invalid and active
    /// leases return Busy. IDs match exactly, without trimming.
    async fn delete_session(&self, session_id: &str) -> Result<bool, StateError>;

    /// Attempt each distinct ID independently, continuing after every failure.
    /// Outside an ambient transaction, successful deletions remain committed.
    /// Inside one, writes join it and storage failures make it rollback-only.
    /// Duplicates are attempted once; empty input returns an empty report.
    async fn delete_sessions(&self, session_ids: &[String]) -> SessionDeletionReport {
        let mut report = SessionDeletionReport::default();
        let mut seen = HashSet::new();
        for session_id in session_ids {
            if !seen.insert(session_id) {
                continue;
            }
            match self.delete_session(session_id).await {
                Ok(true) => report.deleted += 1,
                Ok(false) => {}
                Err(error) => report.failures.push(SessionDeletionFailure {
                    session_id: session_id.clone(),
                    error,
                }),
            }
        }
        report
    }
}

pub struct SessionStoreService(pub Arc<dyn SessionStore>);
pub struct SessionStorePlugin(pub Arc<dyn SessionStore>);

impl SessionStorePlugin {
    pub fn new(store: impl SessionStore + 'static) -> Self {
        Self(Arc::new(store))
    }

    pub fn from_arc(store: Arc<dyn SessionStore>) -> Self {
        Self(store)
    }
}

#[async_trait]
impl Plugin for SessionStorePlugin {
    fn id(&self) -> &str {
        "framework.session-store"
    }

    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        context.provide(SessionStoreService(self.0.clone()))?;
        context.contribute_extension(self.0.clone())
    }
}
