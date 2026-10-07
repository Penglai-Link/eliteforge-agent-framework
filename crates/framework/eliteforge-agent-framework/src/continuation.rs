//! Bounded continuation over native child executions and fenced state journals.
//! A journal is evidence of intent/result, not permission to replay side effects.
use crate::*;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{future::Future, sync::Arc, time::Duration};

tokio::task_local! { static CONTINUATION_PARENT: RunContext; }

/// Explicit native-parent propagation across an adapter future. Spawned tasks do
/// not inherit this scope: capture the parent before spawning, then pass it to
/// `start_child_owned`. This bridge does not reserve or settle any budget itself.
pub async fn with_continuation_parent<T>(
    parent: &RunContext,
    future: impl Future<Output = T>,
) -> T {
    CONTINUATION_PARENT.scope(parent.clone(), future).await
}
pub fn current_continuation_parent() -> Option<RunContext> {
    CONTINUATION_PARENT.try_with(Clone::clone).ok()
}

/// Business policy supplies authorization, prompts and CAS; the driver owns
/// bounded iteration, deadlines, cancellation and durable intent boundaries.
#[async_trait]
pub trait ContinuationPolicy: Send + Sync {
    async fn evaluate(
        &self,
        state: &Value,
        parent: &RunContext,
    ) -> Result<ContinuationDecision, AgentError>;
    async fn execute(
        &self,
        next: Value,
        parent: &RunContext,
    ) -> Result<ContinuationStep, AgentError>;
    /// Persist a product pause/error when an overall SDK ceiling is reached.
    async fn budget_exhausted(&self, state: &Value, parent: &RunContext) -> Result<(), AgentError>;
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContinuationDecision {
    Complete,
    Continue(Value),
}
#[derive(Debug, Clone)]
pub struct ContinuationStep {
    pub state: Value,
    pub content: Content,
    pub interactions: Vec<InteractionRequest>,
    pub status: RunStatus,
}
pub struct ContinuationRequest {
    pub policy: Arc<dyn ContinuationPolicy>,
    pub initial_state: Value,
    pub initial_execution: RunExecutionSnapshot,
    pub max_rounds: usize,
}
impl ContinuationRequest {
    /// Preserve the initial execution's total policy; never multiply it by the
    /// number of rounds. A missing per-step call ceiling is not made unbounded.
    pub fn attach(self, request: &mut RunRequest) -> Result<(), AgentError> {
        if self.max_rounds == 0 {
            return Err(AgentError::InvalidConfiguration(
                "continuation rounds must be positive".into(),
            ));
        }
        if self.initial_execution.incomplete
            && self.initial_execution.historical_calls_bound.is_none()
        {
            return Err(AgentError::InvalidConfiguration("historical execution admission cannot be proven; explicit reconciliation is required".into()));
        }
        if self.initial_execution.child_runs > self.initial_execution.limits.max_child_runs {
            return Err(AgentError::InvalidConfiguration(
                "historical child allowance exceeded".into(),
            ));
        }
        request.limits = self.initial_execution.limits.clone();
        let remaining = self
            .initial_execution
            .deadline_unix_ms
            .saturating_sub(unix_time_ms());
        if remaining == 0 {
            return Err(AgentError::Cancelled);
        }
        request.limits.run_timeout = request
            .limits
            .run_timeout
            .min(Duration::from_millis(remaining));
        let per_step = request.limits.max_model_calls_per_step.ok_or_else(|| {
            AgentError::InvalidConfiguration(
                "continuation requires an explicit model call ceiling".into(),
            )
        })?;
        let max_calls = request
            .limits
            .max_steps
            .checked_mul(per_step)
            .ok_or_else(|| {
                AgentError::InvalidConfiguration("continuation call ceiling overflow".into())
            })?;
        request.extensions.insert(ContinuationBudget::new(
            max_calls,
            request.limits.max_steps,
            request.limits.max_tool_calls,
            self.initial_execution.model_calls,
            self.initial_execution.steps,
            self.initial_execution.tool_calls,
        )?);
        request
            .extensions
            .insert(ModelDispatchCounter::seeded_execution(
                0,
                0,
                0,
                self.initial_execution.opaque,
            ));
        request.extensions.insert(self);
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ContinuationPhase {
    Ready,
    Evaluating,
    Executing,
    Completed,
    AwaitingInput,
    Exhausted,
    Interrupted,
}
/// The opaque state belongs to the policy. No callback or credentials are saved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContinuationJournalState {
    pub phase: ContinuationPhase,
    pub rounds: usize,
    pub state: Value,
    pub pending: Option<Value>,
    pub budget_usage: Usage,
    pub model_calls: usize,
    pub steps: usize,
    pub tool_calls: usize,
    pub child_runs: usize,
    pub child_executions: Vec<RunId>,
    pub deadline_unix_ms: u64,
    pub failure: Option<String>,
}

/// Install on an Agent with a StateStore. Interrupted evaluations/children are
/// recorded, never resumed automatically; hosts may reconcile their own CAS and
/// existing child checkpoints before explicitly requesting fresh work.
pub struct ContinuationLoop;
#[async_trait]
impl AgentLoop for ContinuationLoop {
    fn execution_kind(&self) -> &str {
        "CONTINUATION"
    }
    fn requires_conversations(&self) -> bool {
        false
    }
    fn supports_steering(&self) -> bool {
        false
    }
    fn requires_state(&self) -> bool {
        true
    }
    fn cooperative_cancellation(&self) -> bool {
        true
    }
    async fn run(
        &self,
        request: &RunRequest,
        original: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        if request.resume.is_some() {
            return Err(AgentError::InvalidConfiguration("continuation recovery requires explicit reconciliation; callbacks cannot be replayed".into()));
        }
        let input = request
            .extensions
            .get::<ContinuationRequest>()
            .ok_or_else(|| AgentError::InvalidConfiguration("continuation input missing".into()))?;
        let budget = request
            .extensions
            .get::<ContinuationBudget>()
            .ok_or_else(|| {
                AgentError::InvalidConfiguration("continuation budget missing".into())
            })?;
        let store = services
            .state
            .clone()
            .ok_or(AgentError::MissingService("StateStore"))?;
        let previous = store
            .latest_scope(&request.session_id, request.scope())
            .await?;
        if previous.is_some() {
            return Err(AgentError::Run("this continuation identity was already admitted; inspect its journal rather than replay callbacks".into()));
        }
        let mut context = original.clone();
        context.execution_counters = Some(budget.execution_counters());
        context.child_runs.fetch_max(
            input.initial_execution.child_runs,
            std::sync::atomic::Ordering::AcqRel,
        );
        context
            .usage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .usage = input.initial_execution.budget_usage.clone();
        let deadline_unix_ms = input.initial_execution.deadline_unix_ms;
        let initial = ContinuationJournalState {
            phase: ContinuationPhase::Ready,
            rounds: 0,
            state: input.initial_state.clone(),
            pending: None,
            budget_usage: input.initial_execution.budget_usage.clone(),
            model_calls: budget.calls(),
            steps: budget.steps(),
            tool_calls: budget.tool_calls(),
            child_runs: input.initial_execution.child_runs,
            child_executions: vec![],
            deadline_unix_ms,
            failure: None,
        };
        let mut checkpoint = Checkpoint::new(
            context.run_id,
            &request.session_id,
            "framework.continuation",
            1,
            encode(&initial)?,
        )
        .with_execution_scope(request.scope());
        if let Some(previous) = &previous {
            checkpoint.transcript = previous.transcript.clone();
        }
        checkpoint = store
            .create(checkpoint, previous.map(|checkpoint| checkpoint.run_id))
            .await?;
        let ttl = Duration::from_secs(30);
        let lease = store
            .claim(context.run_id, context.execution_id(), ttl)
            .await?;
        let guard = crate::model_execution::WorkflowLease::new(
            store.clone(),
            lease.clone(),
            ttl,
            request.limits.cleanup_timeout,
            context.clone(),
        );
        let journal = Arc::new(ContinuationJournal {
            store,
            lease,
            checkpoint: tokio::sync::Mutex::new(checkpoint),
            budget: budget.clone(),
            ledger: context.usage.clone(),
            children: context.child_runs.clone(),
        });
        context.budget_journal = Some(journal.clone());
        context
            .extensions
            .insert(ContinuationJournalService(journal.clone()));
        let mut result = drive(input.as_ref(), &budget, &context, &journal).await;
        if let Err(error) = &result {
            // Keep the in-flight phase/pending state visible. Never erase intent
            // or refund an uncertain child merely because the caller disappeared.
            let failure = error.to_string();
            let status = if matches!(error.primary(), AgentError::Cancelled) {
                ExecutionStatus::Cancelled
            } else {
                ExecutionStatus::Failed
            };
            let recorded = journal
                .change(
                    &context,
                    |state| {
                        state.phase = ContinuationPhase::Interrupted;
                        state.failure = Some(failure);
                    },
                    status,
                )
                .await;
            result = finish_with_cleanup(result, recorded);
        }
        let cleanup = guard.close().await.map_err(AgentError::from);
        finish_with_cleanup(result, cleanup)
    }
}
async fn drive(
    input: &ContinuationRequest,
    budget: &ContinuationBudget,
    context: &RunContext,
    journal: &ContinuationJournal,
) -> Result<RunResult, AgentError> {
    let mut state = input.initial_state.clone();
    let mut content = Content::default();
    for round in 0..input.max_rounds {
        context.check_cancelled()?;
        if exhausted(context, budget) {
            input.policy.budget_exhausted(&state, context).await?;
            journal
                .change(
                    context,
                    |journal| journal.phase = ContinuationPhase::Exhausted,
                    ExecutionStatus::Completed,
                )
                .await?;
            return Ok(result(
                context,
                budget,
                content,
                vec![],
                RunStatus::StepLimit,
            ));
        }
        journal
            .change(
                context,
                |journal| {
                    journal.phase = ContinuationPhase::Evaluating;
                    journal.rounds = round;
                    journal.state = state.clone();
                },
                ExecutionStatus::Running,
            )
            .await?;
        let decision = input.policy.evaluate(&state, context).await;
        if decision.is_err() && exhausted(context, budget) && !context.cancellation.is_cancelled() {
            input.policy.budget_exhausted(&state, context).await?;
            journal
                .change(
                    context,
                    |journal| journal.phase = ContinuationPhase::Exhausted,
                    ExecutionStatus::Completed,
                )
                .await?;
            return Ok(result(
                context,
                budget,
                content,
                vec![],
                RunStatus::StepLimit,
            ));
        }
        let decision = decision?;
        let ContinuationDecision::Continue(next) = decision else {
            journal
                .change(
                    context,
                    |journal| journal.phase = ContinuationPhase::Completed,
                    ExecutionStatus::Completed,
                )
                .await?;
            return Ok(result(
                context,
                budget,
                content,
                vec![],
                RunStatus::Completed,
            ));
        };
        context.check_cancelled()?;
        if exhausted(context, budget) {
            input.policy.budget_exhausted(&state, context).await?;
            journal
                .change(
                    context,
                    |journal| journal.phase = ContinuationPhase::Exhausted,
                    ExecutionStatus::Completed,
                )
                .await?;
            return Ok(result(
                context,
                budget,
                content,
                vec![],
                RunStatus::StepLimit,
            ));
        }
        journal
            .change(
                context,
                |journal| {
                    journal.phase = ContinuationPhase::Executing;
                    journal.pending = Some(next.clone());
                },
                ExecutionStatus::Running,
            )
            .await?;
        let outcome = input.policy.execute(next, context).await;
        if outcome.is_err() && exhausted(context, budget) && !context.cancellation.is_cancelled() {
            input.policy.budget_exhausted(&state, context).await?;
            journal
                .change(
                    context,
                    |journal| journal.phase = ContinuationPhase::Exhausted,
                    ExecutionStatus::Completed,
                )
                .await?;
            return Ok(result(
                context,
                budget,
                content,
                vec![],
                RunStatus::StepLimit,
            ));
        }
        let step = outcome?;
        state = step.state;
        content = step.content;
        let stopped = step.status != RunStatus::Completed;
        journal
            .change(
                context,
                |journal| {
                    journal.state = state.clone();
                    journal.pending = None;
                    journal.rounds = round + 1;
                    journal.phase = if step.status == RunStatus::AwaitingInput {
                        ContinuationPhase::AwaitingInput
                    } else if stopped {
                        ContinuationPhase::Exhausted
                    } else {
                        ContinuationPhase::Ready
                    };
                },
                if stopped {
                    ExecutionStatus::Completed
                } else {
                    ExecutionStatus::Running
                },
            )
            .await?;
        if stopped {
            if matches!(step.status, RunStatus::StepLimit | RunStatus::ToolCallLimit) {
                input.policy.budget_exhausted(&state, context).await?;
            }
            return Ok(result(
                context,
                budget,
                content,
                step.interactions,
                step.status,
            ));
        }
    }
    input.policy.budget_exhausted(&state, context).await?;
    journal
        .change(
            context,
            |journal| journal.phase = ContinuationPhase::Exhausted,
            ExecutionStatus::Completed,
        )
        .await?;
    Ok(result(
        context,
        budget,
        content,
        vec![],
        RunStatus::StepLimit,
    ))
}
fn exhausted(context: &RunContext, budget: &ContinuationBudget) -> bool {
    let usage = charged(context);
    budget.exhausted()
        || usage.input_tokens.saturating_add(usage.output_tokens) >= context.token_ceiling
        || context
            .child_runs
            .load(std::sync::atomic::Ordering::Acquire)
            >= context.max_child_runs
}
fn charged(context: &RunContext) -> Usage {
    context
        .usage
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .including_reservations()
}
fn result(
    context: &RunContext,
    budget: &ContinuationBudget,
    content: Content,
    interactions: Vec<InteractionRequest>,
    status: RunStatus,
) -> RunResult {
    RunResult {
        output: content.text(),
        content,
        structured: None,
        interactions,
        status,
        usage: context.usage(),
        steps: budget.steps(),
        tool_calls: budget.tool_calls(),
        external_session: None,
    }
}
fn encode(value: &impl Serialize) -> Result<Value, AgentError> {
    serde_json::to_value(value)
        .map_err(|_| StateError::Invalid("continuation state encoding failed".into()).into())
}
#[derive(Clone)]
pub(crate) struct ContinuationJournalService(pub Arc<ContinuationJournal>);
pub(crate) struct ContinuationJournal {
    store: Arc<dyn StateStore>,
    lease: ExecutionLease,
    checkpoint: tokio::sync::Mutex<Checkpoint>,
    budget: Arc<ContinuationBudget>,
    ledger: Arc<std::sync::Mutex<crate::budget::UsageLedger>>,
    children: Arc<std::sync::atomic::AtomicUsize>,
}
impl ContinuationJournal {
    pub(crate) async fn child_intent(
        &self,
        _context: &RunContext,
        run_id: RunId,
    ) -> Result<(), AgentError> {
        self.commit(
            self.ledger.clone(),
            self.children.clone(),
            |journal| journal.child_executions.push(run_id),
            Some(ExecutionStatus::Running),
        )
        .await
        .map_err(Into::into)
    }
    async fn change(
        &self,
        context: &RunContext,
        update: impl FnOnce(&mut ContinuationJournalState),
        status: ExecutionStatus,
    ) -> Result<(), AgentError> {
        self.commit(
            context.usage.clone(),
            context.child_runs.clone(),
            update,
            Some(status),
        )
        .await
        .map_err(Into::into)
    }
    async fn commit(
        &self,
        ledger: std::sync::Arc<std::sync::Mutex<crate::budget::UsageLedger>>,
        children: Arc<std::sync::atomic::AtomicUsize>,
        update: impl FnOnce(&mut ContinuationJournalState),
        status: Option<ExecutionStatus>,
    ) -> Result<(), StateError> {
        let mut current = self.checkpoint.lock().await;
        let mut state: ContinuationJournalState = serde_json::from_value(current.state.clone())
            .map_err(|_| StateError::Invalid("continuation journal invalid".into()))?;
        state.budget_usage = ledger
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .including_reservations();
        state.model_calls = self.budget.calls();
        state.steps = self.budget.steps();
        state.tool_calls = self.budget.tool_calls();
        state.child_runs = children.load(std::sync::atomic::Ordering::Acquire);
        update(&mut state);
        let mut next = current.clone();
        next.state = serde_json::to_value(state)
            .map_err(|_| StateError::Invalid("continuation journal encoding failed".into()))?;
        if let Some(status) = status {
            next.status = status;
        }
        *current = self
            .store
            .commit(&self.lease, current.revision, next)
            .await?;
        Ok(())
    }
}
#[async_trait]
impl crate::budget::BudgetJournal for ContinuationJournal {
    async fn save(
        &self,
        ledger: std::sync::Arc<std::sync::Mutex<crate::budget::UsageLedger>>,
        children: Arc<std::sync::atomic::AtomicUsize>,
    ) -> Result<(), StateError> {
        self.commit(ledger, children, |_| {}, None).await
    }
}
