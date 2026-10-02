//! Native admission shared by every descendant of a bounded continuation.
use crate::{AgentError, ModelError, RunLimits, Usage, budget::ExecutionCounters};
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

/// Per-execution dispatch observation. It never represents provider billing.
#[derive(Clone, Default)]
pub struct ModelDispatchCounter(Arc<DispatchObservation>);
#[derive(Clone)]
pub struct RestoredExecutionObservation {
    pub limits: RunLimits,
    pub budget_usage: Usage,
    pub steps: usize,
    pub tools: usize,
    pub child_runs: usize,
    pub call_bound: Option<usize>,
}
#[derive(Default)]
struct DispatchObservation {
    calls: AtomicUsize,
    steps: AtomicUsize,
    tools: AtomicUsize,
    opaque: AtomicBool,
    restored: std::sync::Mutex<Option<RestoredExecutionObservation>>,
}
impl ModelDispatchCounter {
    pub fn seeded_execution(calls: usize, steps: usize, tools: usize, opaque: bool) -> Self {
        Self(Arc::new(DispatchObservation {
            calls: AtomicUsize::new(calls),
            steps: AtomicUsize::new(steps),
            tools: AtomicUsize::new(tools),
            opaque: AtomicBool::new(opaque),
            restored: std::sync::Mutex::new(None),
        }))
    }
    pub fn restored(&self) -> Option<RestoredExecutionObservation> {
        self.0
            .restored
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn restore(
        &self,
        limits: RunLimits,
        budget_usage: Usage,
        steps: usize,
        tools: usize,
        child_runs: usize,
        pending_model: bool,
    ) {
        let call_bound = if child_runs > 0 {
            None
        } else {
            limits.max_model_calls_per_step.and_then(|per_step| {
                limits.max_steps.checked_mul(per_step).and_then(|overall| {
                    steps
                        .checked_add(usize::from(pending_model))
                        .and_then(|steps| steps.checked_mul(per_step))
                        .map(|bound| bound.min(overall))
                })
            })
        };
        *self.0.restored.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(RestoredExecutionObservation {
                limits,
                budget_usage,
                steps,
                tools,
                child_runs,
                call_bound,
            });
    }
    pub fn record(&self) {
        self.0.calls.fetch_add(1, Ordering::AcqRel);
    }
    pub fn calls(&self) -> usize {
        self.0.calls.load(Ordering::Acquire)
    }
    /// Count a host-observed opaque attempt, not remote internal requests.
    pub fn record_opaque(&self) {
        self.record();
        self.0.opaque.store(true, Ordering::Release);
    }
    pub fn opaque(&self) -> bool {
        self.0.opaque.load(Ordering::Acquire)
    }
    pub fn record_step(&self) {
        self.0.steps.fetch_add(1, Ordering::AcqRel);
    }
    pub fn record_tools(&self, amount: usize) {
        self.0.tools.fetch_add(amount, Ordering::AcqRel);
    }
    pub fn steps(&self) -> usize {
        self.0.steps.load(Ordering::Acquire)
    }
    pub fn tools(&self) -> usize {
        self.0.tools.load(Ordering::Acquire)
    }
}

/// A tree-wide ceiling. Retry, repair and auxiliary calls spend the same slots.
/// Attach to the root request; native child admission inherits it automatically.
#[derive(Clone)]
pub struct ContinuationBudget {
    calls: Arc<AtomicUsize>,
    max_calls: usize,
    max_steps: usize,
    max_tools: usize,
    counters: Arc<ExecutionCounters>,
}
impl ContinuationBudget {
    pub fn new(
        max_calls: usize,
        max_steps: usize,
        max_tools: usize,
        used_calls: usize,
        used_steps: usize,
        used_tools: usize,
    ) -> Result<Self, AgentError> {
        if max_calls == 0
            || max_steps == 0
            || used_calls > max_calls
            || used_steps > max_steps
            || used_tools > max_tools
        {
            return Err(AgentError::InvalidConfiguration(
                "invalid continuation admission budget".into(),
            ));
        }
        Ok(Self {
            calls: Arc::new(AtomicUsize::new(used_calls)),
            max_calls,
            max_steps,
            max_tools,
            counters: Arc::new(ExecutionCounters {
                steps: AtomicUsize::new(used_steps),
                tools: AtomicUsize::new(used_tools),
            }),
        })
    }
    pub fn admit_model_call(&self) -> Result<(), ModelError> {
        self.calls
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                (used < self.max_calls).then(|| used + 1)
            })
            .map(|_| ())
            .map_err(|_| ModelError::Fatal("continuation model call budget exhausted".into()))
    }
    /// Local DAG counters and the continuation root keep their own ceilings.
    /// A rejection before execution releases only this provisional admission.
    pub fn admit_step(&self, local: Option<&Arc<ExecutionCounters>>, local_limit: usize) -> bool {
        self.admit(local, local_limit, 1, false)
    }
    pub fn admit_tools(
        &self,
        local: Option<&Arc<ExecutionCounters>>,
        local_limit: usize,
        amount: usize,
    ) -> bool {
        self.admit(local, local_limit, amount, true)
    }
    fn admit(
        &self,
        local: Option<&Arc<ExecutionCounters>>,
        local_limit: usize,
        amount: usize,
        tools: bool,
    ) -> bool {
        let other = local.filter(|counter| !Arc::ptr_eq(counter, &self.counters));
        if let Some(other) = other {
            let counter = if tools { &other.tools } else { &other.steps };
            if !ExecutionCounters::admit(counter, amount, local_limit) {
                return false;
            }
        }
        let counter = if tools {
            &self.counters.tools
        } else {
            &self.counters.steps
        };
        if ExecutionCounters::admit(
            counter,
            amount,
            if tools {
                self.max_tools
            } else {
                self.max_steps
            },
        ) {
            true
        } else {
            if let Some(other) = other {
                let counter = if tools { &other.tools } else { &other.steps };
                counter.fetch_sub(amount, Ordering::AcqRel);
            }
            false
        }
    }
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::Acquire)
    }
    pub fn steps(&self) -> usize {
        self.counters.steps.load(Ordering::Acquire)
    }
    pub fn tool_calls(&self) -> usize {
        self.counters.tools.load(Ordering::Acquire)
    }
    pub fn execution_counters(&self) -> Arc<ExecutionCounters> {
        self.counters.clone()
    }
    pub fn apply_limits(&self, limits: &mut RunLimits) {
        limits.max_steps = limits.max_steps.min(self.max_steps);
        limits.max_tool_calls = limits.max_tool_calls.min(self.max_tools);
    }
    pub fn exhausted(&self) -> bool {
        self.calls() >= self.max_calls || self.steps() >= self.max_steps
    }
}

/// Native usage/calls used to seed a continuation after an already completed turn.
/// `budget_usage` includes conservative reservations, and is never billed usage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunExecutionSnapshot {
    pub limits: RunLimits,
    pub budget_usage: Usage,
    /// Conservative charged dispatch slots; historical estimates are distinct
    /// from the observed dispatch events/provider reports.
    pub model_calls: usize,
    #[serde(default)]
    pub incomplete: bool,
    #[serde(default)]
    pub historical_calls_bound: Option<usize>,
    #[serde(default)]
    pub child_runs: usize,
    pub steps: usize,
    pub tool_calls: usize,
    pub deadline_unix_ms: u64,
    /// Opaque external execution; remote internal dispatches are not controllable.
    pub opaque: bool,
}
impl PartialEq for RunExecutionSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.incomplete == other.incomplete
            && self.historical_calls_bound == other.historical_calls_bound
            && self.child_runs == other.child_runs
            && self.opaque == other.opaque
            && self.budget_usage == other.budget_usage
            && self.model_calls == other.model_calls
            && self.steps == other.steps
            && self.tool_calls == other.tool_calls
            && self.deadline_unix_ms == other.deadline_unix_ms
            && serde_json::to_value(&self.limits).ok() == serde_json::to_value(&other.limits).ok()
    }
}
