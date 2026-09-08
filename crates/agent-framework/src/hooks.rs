//! Lifecycle notifications, separate from execution interception.
use crate::{AgentError, AgentEvent, RunContext, RunId, RunResult};
use async_trait::async_trait;
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};

/// Read-only identity. Hooks receive no execution continuation or mutable result.
pub struct RunLifecycleContext<'a> {
    pub run_id: RunId,
    pub execution_id: RunId,
    pub session_id: &'a str,
}

/// Notifications are attempted once per admitted execution, including resume
/// and middleware denial. Start callbacks may be skipped after cancellation or
/// deadline expiry; finish callbacks are still attempted with a bounded wait.
/// These are notifications, not resource finalizers or authorization checks.
/// Hook failures are isolated and reported as events without changing the run.
#[async_trait]
pub trait RunLifecycleHook: Send + Sync {
    async fn on_started(&self, _context: &RunLifecycleContext<'_>) {}
    async fn on_finished(
        &self,
        _context: &RunLifecycleContext<'_>,
        _result: &Result<RunResult, AgentError>,
    ) {
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum RunLifecyclePhase {
    Started,
    Finished,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum LifecycleHookFailure {
    Panicked,
    TimedOut,
    Interrupted,
}

pub(crate) async fn notify_started(
    hooks: &[Arc<dyn RunLifecycleHook>],
    run: &RunContext,
    timeout: Duration,
) {
    notify(hooks, run, None, timeout).await;
}
pub(crate) async fn notify_finished(
    hooks: &[Arc<dyn RunLifecycleHook>],
    run: &RunContext,
    result: &Result<RunResult, AgentError>,
    timeout: Duration,
) {
    notify(hooks, run, Some(result), timeout).await;
}
async fn notify(
    hooks: &[Arc<dyn RunLifecycleHook>],
    run: &RunContext,
    result: Option<&Result<RunResult, AgentError>>,
    timeout: Duration,
) {
    let context = RunLifecycleContext {
        run_id: run.run_id,
        execution_id: run.execution_id(),
        session_id: &run.session_id,
    };
    let phase = if result.is_some() {
        RunLifecyclePhase::Finished
    } else {
        RunLifecyclePhase::Started
    };
    for (hook_index, hook) in hooks.iter().enumerate() {
        let callback = std::panic::AssertUnwindSafe(async {
            if let Some(result) = result {
                hook.on_finished(&context, result).await;
            } else {
                hook.on_started(&context).await;
            }
        })
        .catch_unwind();
        let failure = tokio::select! {
            biased;
            _ = run.cancelled(), if result.is_none() => Some(LifecycleHookFailure::Interrupted),
            _ = tokio::time::sleep_until(run.deadline), if result.is_none() => Some(LifecycleHookFailure::TimedOut),
            outcome = tokio::time::timeout(timeout, callback) => match outcome {
                Ok(Ok(())) => None, Ok(Err(_)) => Some(LifecycleHookFailure::Panicked), Err(_) => Some(LifecycleHookFailure::TimedOut),
            },
        };
        if let Some(failure) = failure {
            run.emit(AgentEvent::LifecycleHookFailed {
                phase,
                hook_index,
                failure,
            })
            .await;
        }
    }
}
