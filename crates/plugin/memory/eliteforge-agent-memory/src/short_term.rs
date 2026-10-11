//! Scoped block sources and assembly. Host DTOs are opaque to the memory engine.
use crate::ModelContextBudgetPlan;
use eliteforge_agent_core::MemoryError;
/// Platform memory has its own identity, independent of any workspace/namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryScopeKind {
    Namespace,
    Platform,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryScope {
    kind: MemoryScopeKind,
    namespace: String,
    session: String,
}
impl MemoryScope {
    pub fn new(
        namespace: impl Into<String>,
        session: impl Into<String>,
    ) -> Result<Self, MemoryError> {
        let namespace = namespace.into();
        let session = session.into();
        if namespace.trim().is_empty() || session.trim().is_empty() {
            return Err(MemoryError::Invalid("memory scope is empty".into()));
        }
        Ok(Self {
            kind: MemoryScopeKind::Namespace,
            namespace,
            session,
        })
    }
    /// A platform session needs no synthetic workspace record. The kind is part
    /// of scope equality, so a host namespace called `platform` cannot collide.
    pub fn platform(session: impl Into<String>) -> Result<Self, MemoryError> {
        let session = session.into();
        if session.trim().is_empty() {
            return Err(MemoryError::Invalid("memory session is empty".into()));
        }
        Ok(Self {
            kind: MemoryScopeKind::Platform,
            namespace: "platform".into(),
            session,
        })
    }
    pub fn kind(&self) -> MemoryScopeKind {
        self.kind
    }
    pub fn is_platform(&self) -> bool {
        self.kind == MemoryScopeKind::Platform
    }
    /// Stable, unambiguous key for caches and host persistence. Includes the
    /// scope kind so platform sessions cannot reuse a namespace's summaries.
    pub fn storage_key(&self) -> String {
        let kind = match self.kind {
            MemoryScopeKind::Namespace => "namespace",
            MemoryScopeKind::Platform => "platform",
        };
        format!(
            "{}:{}:{}:{}:{}",
            kind,
            self.namespace.len(),
            self.namespace,
            self.session.len(),
            self.session
        )
    }
    /// Hosts must include `kind()` alongside this label in persisted scope keys.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }
    pub fn session(&self) -> &str {
        &self.session
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryRevision {
    pub scope: MemoryScope,
    pub generation: String,
    pub revision: u64,
}
impl MemoryRevision {
    pub fn ensure_current(&self, current: &Self) -> Result<(), MemoryError> {
        if self != current {
            Err(MemoryError::Conflict)
        } else {
            Ok(())
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemoryBlockKind {
    SeedPromptPrefix,
    CurrentSessionCheckpoint,
    SessionFamilyContext,
    CurrentSessionWorkingState,
    CurrentSessionHistory,
}
impl MemoryBlockKind {
    pub fn id(self) -> &'static str {
        match self {
            Self::SeedPromptPrefix => "seed_prompt_prefix",
            Self::CurrentSessionCheckpoint => "current_session_checkpoint",
            Self::SessionFamilyContext => "session_family_context",
            Self::CurrentSessionWorkingState => "current_session_working_state",
            Self::CurrentSessionHistory => "current_session_history",
        }
    }
}
#[derive(Debug, Clone)]
pub struct MemoryBlock<T> {
    pub kind: MemoryBlockKind,
    pub entries: Vec<T>,
}
#[derive(Debug, Clone)]
pub struct MemorySnapshot<T> {
    pub revision: MemoryRevision,
    pub blocks: Vec<MemoryBlock<T>>,
}
/// Hosts expose only authorized, model-visible data under a bound namespace.
pub trait ShortTermMemorySource<T> {
    fn read(&self, scope: &MemoryScope) -> Result<MemorySnapshot<T>, MemoryError>;
}
/// Projection persistence must check revision and write in the same host
/// transaction/lease. It never replaces the authoritative transcript.
pub trait ShortTermMemoryProjectionStore<T> {
    fn compare_exchange_projection(
        &mut self,
        expected: &MemoryRevision,
        projection: T,
    ) -> Result<(), MemoryError>;
}
#[derive(Debug, Clone)]
pub struct MemoryBlockReport {
    pub kind: MemoryBlockKind,
    pub tokens: u32,
    pub budget_tokens: u32,
    pub entries: usize,
}
pub struct AssembledMemory<T> {
    pub messages: Vec<T>,
    pub reports: Vec<MemoryBlockReport>,
    pub revision: MemoryRevision,
}
/// One assembly order and one budget policy; hosts supply only DTO mapping.
pub struct ShortTermMemoryEngine;
impl ShortTermMemoryEngine {
    /// `bound` must preserve framing/identity and produce deterministic,
    /// monotonically nondecreasing estimates as its requested target grows.
    /// Optional blocks use a finite binary search for the largest fitting target.
    /// Seed instructions and current history are mandatory even if disabled in
    /// the plan; their complete estimates remain in reports for model admission.
    /// A host must reject an over-budget request before executing its model.
    pub fn assemble<T: Clone>(
        source: &dyn ShortTermMemorySource<T>,
        scope: &MemoryScope,
        plan: &ModelContextBudgetPlan,
        estimate: impl Fn(&T) -> u32,
        bound: impl Fn(T, u32) -> T,
    ) -> Result<AssembledMemory<T>, MemoryError> {
        let snapshot = source.read(scope)?;
        if snapshot.revision.scope != *scope {
            return Err(MemoryError::Invalid(
                "memory source crossed its scope".into(),
            ));
        }
        let order = [
            MemoryBlockKind::SeedPromptPrefix,
            MemoryBlockKind::CurrentSessionCheckpoint,
            MemoryBlockKind::SessionFamilyContext,
            MemoryBlockKind::CurrentSessionWorkingState,
            MemoryBlockKind::CurrentSessionHistory,
        ];
        let mut messages = Vec::new();
        let mut reports = Vec::new();
        for kind in order {
            let config = match kind {
                MemoryBlockKind::SeedPromptPrefix => &plan.block_budgets.seed_prompt_prefix,
                MemoryBlockKind::CurrentSessionCheckpoint => {
                    &plan.block_budgets.current_session_checkpoint
                }
                MemoryBlockKind::SessionFamilyContext => &plan.block_budgets.session_family_context,
                MemoryBlockKind::CurrentSessionWorkingState => {
                    &plan.block_budgets.current_session_working_state
                }
                MemoryBlockKind::CurrentSessionHistory => {
                    &plan.block_budgets.current_session_history
                }
            };
            let mut tokens = 0u32;
            let mut entries = 0usize;
            for block in snapshot.blocks.iter().filter(|b| b.kind == kind) {
                for entry in &block.entries {
                    if !config.enabled
                        && !matches!(
                            kind,
                            MemoryBlockKind::SeedPromptPrefix
                                | MemoryBlockKind::CurrentSessionHistory
                        )
                    {
                        continue;
                    }
                    let mut item = entry.clone();
                    if matches!(
                        kind,
                        MemoryBlockKind::CurrentSessionCheckpoint
                            | MemoryBlockKind::SessionFamilyContext
                            | MemoryBlockKind::CurrentSessionWorkingState
                    ) {
                        let remaining = config.effective_limit_tokens.saturating_sub(tokens);
                        item = bound(entry.clone(), remaining);
                        if estimate(&item) > remaining {
                            let mut low = 0u32;
                            let mut high = remaining;
                            let mut best = bound(entry.clone(), 0);
                            if estimate(&best) > remaining {
                                continue;
                            }
                            while low < high {
                                let midpoint = low + (high - low) / 2 + (high - low) % 2;
                                let candidate = bound(entry.clone(), midpoint);
                                if estimate(&candidate) <= remaining {
                                    low = midpoint;
                                    best = candidate;
                                } else {
                                    high = midpoint - 1;
                                }
                            }
                            item = best;
                        }
                    }
                    tokens = tokens.saturating_add(estimate(&item));
                    entries += 1;
                    messages.push(item);
                }
            }
            reports.push(MemoryBlockReport {
                kind,
                tokens,
                budget_tokens: config.effective_limit_tokens,
                entries,
            });
        }
        Ok(AssembledMemory {
            messages,
            reports,
            revision: snapshot.revision,
        })
    }
    pub fn write_projection<T>(
        store: &mut dyn ShortTermMemoryProjectionStore<T>,
        expected: &MemoryRevision,
        projection: T,
    ) -> Result<(), MemoryError> {
        store.compare_exchange_projection(expected, projection)
    }
}
/// Generic snapshots are themselves immutable sources; useful for host mappings.
impl<T: Clone> ShortTermMemorySource<T> for MemorySnapshot<T> {
    fn read(&self, scope: &MemoryScope) -> Result<MemorySnapshot<T>, MemoryError> {
        if self.revision.scope != *scope {
            return Err(MemoryError::Invalid("memory snapshot crossed scope".into()));
        }
        Ok(self.clone())
    }
}
/// Count every semantic field rather than ignoring tool arguments or files.
pub fn estimate_serialized_tokens<T: serde::Serialize>(entry: &T) -> u32 {
    serde_json::to_vec(entry)
        .map(|v| v.len().saturating_add(16).min(u32::MAX as usize) as u32)
        .unwrap_or(u32::MAX)
}

#[cfg(test)]
mod platform_scope_tests {
    use super::*;
    #[test]
    fn platform_scope_is_explicit_and_cannot_alias_a_host_namespace() {
        let platform = MemoryScope::platform("session").unwrap();
        assert!(platform.is_platform());
        assert_eq!(platform.kind(), MemoryScopeKind::Platform);
        assert_ne!(platform, MemoryScope::new("platform", "session").unwrap());
        assert_ne!(
            platform.storage_key(),
            MemoryScope::new("platform", "session")
                .unwrap()
                .storage_key()
        );
        assert!(MemoryScope::platform(" ").is_err());
        assert!(MemoryScope::new("", "session").is_err());
        let original = MemoryRevision {
            scope: platform,
            generation: "g".into(),
            revision: 1,
        };
        let other = MemoryRevision {
            scope: MemoryScope::new("platform", "session").unwrap(),
            generation: "g".into(),
            revision: 1,
        };
        assert!(matches!(
            original.ensure_current(&other),
            Err(MemoryError::Conflict)
        ));
    }
}
