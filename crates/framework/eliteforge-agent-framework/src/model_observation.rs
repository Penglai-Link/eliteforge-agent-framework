//! Bounded diagnostics and accounting, independent of provider-reported actual usage.
use crate::*;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ModelExecutionReport {
    /// Calls admitted to a model adapter; this is not a billable HTTP guarantee.
    pub calls: usize,
    pub attempts: usize,
    pub retries: usize,
    pub ledger_usage: Usage,
    /// Complete actual usage only when every dispatched call reported it.
    pub provider_usage: Option<Usage>,
    /// Known successful provider usage; never a complete billing assertion.
    pub provider_usage_subtotal: Option<Usage>,
    pub provider_metrics: Option<ProviderMetrics>,
    pub selected: Vec<ModelRef>,
    pub dispatches: Vec<ModelDispatchReport>,
    pub diagnostics_truncated: bool,
}
pub struct ObservedModelResult<T> {
    pub result: Result<T, AgentError>,
    pub report: ModelExecutionReport,
}
impl<T> ObservedModelResult<T> {
    pub fn into_result(self) -> Result<T, AgentError> {
        self.result
    }
    pub fn map<U>(self, map: impl FnOnce(T) -> U) -> ObservedModelResult<U> {
        ObservedModelResult {
            result: self.result.map(map),
            report: self.report,
        }
    }
}
#[derive(Default)]
pub(crate) struct ModelReportState {
    report: Mutex<ModelExecutionReport>,
    ledger: Mutex<Option<Arc<Mutex<eliteforge_agent_core::budget::UsageLedger>>>>,
    reported: Mutex<usize>,
}
impl ModelReportState {
    pub fn attach(self: &Arc<Self>, context: &RunContext) -> RunContext {
        *self.ledger.lock().unwrap_or_else(|e| e.into_inner()) = Some(context.usage.clone());
        let mut context = context.clone();
        context.sinks.push(self.clone());
        context.extensions.insert(self.clone());
        context
    }
    pub fn response(&self, response: &ChatResponse) {
        if response.usage == Usage::default() {
            return;
        }
        *self.reported.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        let mut report = self.report.lock().unwrap_or_else(|e| e.into_inner());
        let first = report.provider_usage_subtotal.is_none();
        let subtotal = report
            .provider_usage_subtotal
            .get_or_insert_with(Usage::default);
        subtotal.input_tokens = subtotal
            .input_tokens
            .saturating_add(response.usage.input_tokens);
        subtotal.output_tokens = subtotal
            .output_tokens
            .saturating_add(response.usage.output_tokens);
        let metrics = response.provider_metrics();
        report.provider_metrics = if first {
            metrics
        } else {
            match (report.provider_metrics.take(), metrics) {
                (Some(left), Some(right)) => Some(ProviderMetrics {
                    cached_input_tokens: sum_known(
                        left.cached_input_tokens,
                        right.cached_input_tokens,
                    ),
                    reasoning_tokens: sum_known(left.reasoning_tokens, right.reasoning_tokens),
                }),
                _ => None,
            }
        };
    }
    pub fn operation_usage(&self, usage: Option<Usage>) {
        if let Some(usage) = usage {
            self.response(&ChatResponse {
                usage,
                ..Default::default()
            });
        }
    }
    pub fn calls(&self) -> usize {
        self.report.lock().unwrap_or_else(|e| e.into_inner()).calls
    }
    pub fn snapshot(&self, attempts: usize) -> ModelExecutionReport {
        let mut report = self
            .report
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        report.attempts = attempts;
        if let Some(ledger) = self
            .ledger
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            report.ledger_usage = ledger
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .including_reservations();
        }
        let reported = *self.reported.lock().unwrap_or_else(|e| e.into_inner());
        report.provider_usage = report
            .provider_usage_subtotal
            .clone()
            .filter(|_| reported == report.calls && report.calls > 0);
        if report.provider_usage.is_none() {
            report.provider_metrics = None;
        }
        report
    }
}
fn sum_known(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    Some(left?.saturating_add(right?))
}
#[async_trait]
impl EventSink for ModelReportState {
    fn on_published(&self, _: &RunContext, envelope: &EventEnvelope) {
        if let AgentEvent::ModelUsage { stage, usage, .. } = &envelope.event
            && stage == "rerank"
        {
            self.operation_usage(Some(usage.clone()));
        }
        let mut report = self.report.lock().unwrap_or_else(|e| e.into_inner());
        match &envelope.event {
            AgentEvent::ModelSelected { model, .. } => {
                if report.selected.len() < 256 {
                    report.selected.push(model.clone());
                } else {
                    report.diagnostics_truncated = true;
                }
            }
            AgentEvent::ModelDispatched(dispatch) => {
                report.calls = report.calls.saturating_add(1);
                if report.dispatches.len() < 256 {
                    report.dispatches.push(dispatch.clone());
                } else {
                    report.diagnostics_truncated = true;
                }
            }
            AgentEvent::ModelOperationStarted { model, .. } => {
                report.calls = report.calls.saturating_add(1);
                if report.selected.len() < 256 {
                    report.selected.push(model.clone());
                } else {
                    report.diagnostics_truncated = true;
                }
            }
            AgentEvent::ModelRetry { .. } => report.retries = report.retries.saturating_add(1),
            _ => {}
        }
    }
    async fn on_event(&self, _: &RunContext, _: &AgentEvent) {}
}
