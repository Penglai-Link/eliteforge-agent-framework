//! Host-neutral telemetry records and storage port. No SDK, transport or business scope.
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc, time::Duration};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetrySpan {
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub name: String,
    pub started_at: i64,
    pub ended_at: i64,
    pub status_code: i32,
    pub attributes: BTreeMap<String, String>,
    pub events: Vec<TelemetrySpanEvent>,
    pub links: Vec<TelemetrySpanLink>,
    /// Original OTLP span, resource and instrumentation scope, including events and links.
    pub data: serde_json::Value,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryTrace {
    pub trace_id: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub status_code: i32,
    pub attributes: BTreeMap<String, String>,
    pub abandoned: bool,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryMetricPoint {
    pub point_id: String,
    pub name: String,
    pub description: String,
    pub unit: String,
    pub kind: String,
    pub temporality: i32,
    pub monotonic: bool,
    pub timestamp: i64,
    pub resource: BTreeMap<String, String>,
    pub scope: String,
    pub data: serde_json::Value,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TelemetrySpanEvent {
    pub name: String,
    pub timestamp: i64,
    pub attributes: BTreeMap<String, String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TelemetrySpanLink {
    pub trace_id: String,
    pub span_id: String,
}
#[derive(Clone, Debug, Default)]
pub struct TelemetryTraceFilter {
    pub trace_id: Option<String>,
    pub attributes: BTreeMap<String, String>,
}
#[derive(Clone, Debug)]
pub struct TelemetryPage<T> {
    pub total: usize,
    pub records: Vec<T>,
}
#[derive(Clone, Debug)]
pub struct TelemetryRetention {
    pub trace_max_age: Duration,
    pub metric_max_age: Duration,
    pub max_completed_traces: Option<usize>,
}
impl TelemetryRetention {
    pub fn validate(&self) -> Result<(), TelemetryError> {
        if self.trace_max_age.as_millis() == 0
            || self.metric_max_age.as_millis() == 0
            || self.trace_max_age.as_millis() > i64::MAX as u128
            || self.metric_max_age.as_millis() > i64::MAX as u128
            || self.max_completed_traces == Some(0)
        {
            return Err(TelemetryError::Invalid("invalid retention policy".into()));
        }
        Ok(())
    }
}
#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    #[error("invalid telemetry: {0}")]
    Invalid(String),
    #[error("telemetry storage: {0}")]
    Storage(String),
}
#[async_trait]
pub trait TelemetryStore: Send + Sync {
    async fn register_trace(&self, trace: TelemetryTrace) -> Result<(), TelemetryError>;
    async fn write_spans(&self, spans: Vec<TelemetrySpan>) -> Result<(), TelemetryError>;
    async fn write_metrics(&self, points: Vec<TelemetryMetricPoint>) -> Result<(), TelemetryError>;
    async fn query_traces(
        &self,
        filter: TelemetryTraceFilter,
        offset: usize,
        limit: usize,
    ) -> Result<TelemetryPage<TelemetryTrace>, TelemetryError>;
    async fn spans(&self, trace_id: String) -> Result<Vec<TelemetrySpan>, TelemetryError>;
    /// Sum requested numeric span attributes; the caller supplies semantic attribute names.
    async fn sum_span_attributes(
        &self,
        filter: TelemetryTraceFilter,
        span_name: String,
        attributes: Vec<String>,
    ) -> Result<BTreeMap<String, u64>, TelemetryError>;
    async fn metric_points(
        &self,
        name: Option<String>,
        offset: usize,
        limit: usize,
    ) -> Result<TelemetryPage<TelemetryMetricPoint>, TelemetryError>;
    /// Count metric points whose resource or point attributes match the given key and value.
    async fn count_metric_points_by_attribute(
        &self,
        attribute: String,
        value: String,
    ) -> Result<usize, TelemetryError>;
    async fn latest_metric(
        &self,
        name: String,
        resource: BTreeMap<String, String>,
    ) -> Result<Option<TelemetryMetricPoint>, TelemetryError>;
    async fn cleanup(&self) -> Result<(), TelemetryError>;
}
#[derive(Clone)]
pub struct TelemetryStoreService(pub Arc<dyn TelemetryStore>);
