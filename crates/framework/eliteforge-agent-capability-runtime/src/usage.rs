use crate::{RuntimeError, RuntimeResult};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UsageBlocker {
    pub package: String,
    pub version: String,
    pub scope: String,
    pub kind: String,
    pub label: String,
}
#[derive(Default)]
struct UsageState {
    next: u64,
    leases: BTreeMap<u64, UsageBlocker>,
    updating: std::collections::BTreeSet<(String, Option<String>)>,
}
#[derive(Clone, Default)]
pub struct UsageRegistry {
    state: Arc<Mutex<UsageState>>,
}
pub struct UsageLease {
    registry: UsageRegistry,
    id: u64,
}
pub struct UpdateLease {
    registry: UsageRegistry,
    packages: Vec<String>,
    scope: Option<String>,
}
impl UsageRegistry {
    pub fn acquire(&self, blocker: UsageBlocker) -> RuntimeResult<UsageLease> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.updating.contains(&(blocker.package.clone(), None))
            || (state
                .updating
                .contains(&(blocker.package.clone(), Some(blocker.scope.clone())))
                && !matches!(
                    blocker.kind.as_str(),
                    "selection" | "runtime" | "extension_call"
                ))
        {
            return Err(RuntimeError::Busy(vec![blocker]));
        }
        state.next += 1;
        let id = state.next;
        state.leases.insert(id, blocker);
        Ok(UsageLease {
            registry: self.clone(),
            id,
        })
    }
    pub fn all_blockers(&self) -> Vec<UsageBlocker> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .leases
            .values()
            .cloned()
            .collect()
    }
    pub fn blockers(&self, package: &str) -> Vec<UsageBlocker> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .leases
            .values()
            .filter(|u| u.package == package)
            .cloned()
            .collect()
    }
    pub fn begin_update(&self, packages: Vec<String>) -> RuntimeResult<UpdateLease> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let blockers: Vec<_> = state
            .leases
            .values()
            .filter(|u| packages.contains(&u.package))
            .cloned()
            .collect();
        if !blockers.is_empty() {
            return Err(RuntimeError::Busy(blockers));
        }
        if packages
            .iter()
            .any(|p| state.updating.iter().any(|(package, _)| package == p))
        {
            return Err(RuntimeError::Invalid(
                "package update already in progress".into(),
            ));
        }
        state
            .updating
            .extend(packages.iter().cloned().map(|package| (package, None)));
        Ok(UpdateLease {
            registry: self.clone(),
            packages,
            scope: None,
        })
    }
    pub fn begin_version_removal(
        &self,
        package: &str,
        version: &str,
    ) -> RuntimeResult<UpdateLease> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let blockers: Vec<_> = state
            .leases
            .values()
            .filter(|lease| lease.package == package && lease.version == version)
            .cloned()
            .collect();
        if !blockers.is_empty() {
            return Err(RuntimeError::Busy(blockers));
        }
        if state.updating.iter().any(|(id, _)| id == package) {
            return Err(RuntimeError::Invalid("package update in progress".into()));
        }
        state.updating.insert((package.into(), None));
        Ok(UpdateLease {
            registry: self.clone(),
            packages: vec![package.into()],
            scope: None,
        })
    }
    pub fn begin_scope_update(&self, package: &str, scope: &str) -> RuntimeResult<UpdateLease> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let blockers: Vec<_> = state
            .leases
            .values()
            .filter(|lease| {
                lease.package == package
                    && lease.scope == scope
                    && !matches!(lease.kind.as_str(), "selection" | "runtime")
            })
            .cloned()
            .collect();
        if !blockers.is_empty() {
            return Err(RuntimeError::Busy(blockers));
        }
        let key = (package.to_owned(), Some(scope.to_owned()));
        if state.updating.contains(&(package.to_owned(), None)) || !state.updating.insert(key) {
            return Err(RuntimeError::Invalid(
                "scope update already in progress".into(),
            ));
        }
        Ok(UpdateLease {
            registry: self.clone(),
            packages: vec![package.to_owned()],
            scope: Some(scope.to_owned()),
        })
    }
    pub(crate) fn release_scope(&self, package: &str, version: &str, scope: &str) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .leases
            .retain(|_, lease| {
                !(lease.package == package && lease.version == version && lease.scope == scope)
            });
    }
}
impl Drop for UsageLease {
    fn drop(&mut self) {
        self.registry
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .leases
            .remove(&self.id);
    }
}
impl Drop for UpdateLease {
    fn drop(&mut self) {
        let mut state = self
            .registry
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for package in &self.packages {
            state
                .updating
                .remove(&(package.clone(), self.scope.clone()));
        }
    }
}
