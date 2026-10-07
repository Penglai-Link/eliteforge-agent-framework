use crate::*;
use async_trait::async_trait;
use std::{sync::Arc, time::Duration};
pub(crate) struct CapabilityBoundState {
    pub store: Arc<dyn StateStore>,
    pub packs: Vec<Arc<dyn CapabilityPack>>,
    pub versions: Vec<CapabilityVersionBinding>,
}
impl CapabilityBoundState {
    fn validate(&self, checkpoint: &Checkpoint) -> Result<(), StateError> {
        if !checkpoint.status.is_terminal() && checkpoint.capability_versions != self.versions {
            return Err(StateError::Invalid(
                "checkpoint requires a different capability package selection".into(),
            ));
        }
        Ok(())
    }
    async fn protect(&self, run: RunId) -> Result<(), StateError> {
        for pack in &self.packs {
            pack.protect_task(run)
                .await
                .map_err(|e| StateError::Backend(e.to_string()))?;
        }
        Ok(())
    }
    async fn release_pins(&self, run: RunId) -> Result<(), StateError> {
        for pack in &self.packs {
            pack.release_task(run).await.map_err(|e| {
                StateError::Backend(format!(
                    "terminal checkpoint committed but capability pin cleanup failed: {e}"
                ))
            })?;
        }
        Ok(())
    }
}
#[async_trait]
impl StateStore for CapabilityBoundState {
    async fn create(
        &self,
        mut checkpoint: Checkpoint,
        previous_run: Option<RunId>,
    ) -> Result<Checkpoint, StateError> {
        checkpoint.capability_versions = self.versions.clone();
        self.protect(checkpoint.run_id).await?;
        self.store.create(checkpoint, previous_run).await
    }
    async fn load(&self, run: RunId) -> Result<Checkpoint, StateError> {
        let checkpoint = self.store.load(run).await?;
        self.validate(&checkpoint)?;
        Ok(checkpoint)
    }
    async fn latest(&self, session: &str) -> Result<Option<Checkpoint>, StateError> {
        let checkpoint = self.store.latest(session).await?;
        if let Some(checkpoint) = &checkpoint {
            self.validate(checkpoint)?;
        }
        Ok(checkpoint)
    }
    async fn latest_scope(
        &self,
        session: &str,
        scope: &str,
    ) -> Result<Option<Checkpoint>, StateError> {
        let checkpoint = self.store.latest_scope(session, scope).await?;
        if let Some(checkpoint) = &checkpoint {
            self.validate(checkpoint)?;
        }
        Ok(checkpoint)
    }
    async fn claim(
        &self,
        run: RunId,
        owner: RunId,
        ttl: Duration,
    ) -> Result<ExecutionLease, StateError> {
        self.load(run).await?;
        self.store.claim(run, owner, ttl).await
    }
    async fn renew(
        &self,
        lease: &ExecutionLease,
        ttl: Duration,
    ) -> Result<ExecutionLease, StateError> {
        self.store.renew(lease, ttl).await
    }
    async fn commit(
        &self,
        lease: &ExecutionLease,
        revision: u64,
        mut next: Checkpoint,
    ) -> Result<Checkpoint, StateError> {
        if next.capability_versions != self.versions && !next.capability_versions.is_empty() {
            return Err(StateError::Invalid(
                "cannot change checkpoint capability versions".into(),
            ));
        }
        next.capability_versions = self.versions.clone();
        self.protect(next.run_id).await?;
        let committed = self.store.commit(lease, revision, next).await?;
        if committed.status.is_terminal() {
            self.release_pins(committed.run_id).await?;
        }
        Ok(committed)
    }
    async fn release(&self, lease: &ExecutionLease) -> Result<(), StateError> {
        self.store.release(lease).await
    }
}
