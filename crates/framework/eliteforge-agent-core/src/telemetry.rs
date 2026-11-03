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
/// Match each attribute in either the resource or the OTLP point attribute list.
#[derive(Clone, Debug, Default)]
pub struct TelemetryMetricFilter {
    pub name: Option<String>,
    pub attributes: BTreeMap<String, String>,
}
impl From<Option<String>> for TelemetryMetricFilter {
    fn from(name: Option<String>) -> Self {
        Self {
            name,
            ..Default::default()
        }
    }
}
/// Generic trace predicates. Hosts supply their classification semantics.
#[derive(Clone, Debug)]
pub enum TelemetryTracePredicate {
    Attribute { key: String, value: String },
    StatusCode(i32),
    HasEnded(bool),
    Abandoned(bool),
    All(Vec<Self>),
    Any(Vec<Self>),
    Not(Box<Self>),
}
#[derive(Clone, Debug)]
pub struct TelemetryBucketQuery {
    pub start: i64,
    pub end: i64,
    pub width_ms: i64,
}
#[derive(Clone, Debug, Default)]
pub struct TelemetryTraceStatistics {
    pub total: usize,
    pub classified: BTreeMap<String, usize>,
    pub average_duration_ms: f64,
    pub p95_duration_ms: f64,
    pub buckets: Vec<TelemetryTraceBucket>,
}
#[derive(Clone, Debug)]
pub struct TelemetryTraceBucket {
    pub started_at: i64,
    pub total: usize,
    pub classified: BTreeMap<String, usize>,
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
    /// Permanently remove all persisted telemetry, including derived indexes and rollups.
    async fn clear(&self) -> Result<(), TelemetryError>;
    async fn register_trace(&self, trace: TelemetryTrace) -> Result<(), TelemetryError>;
    async fn write_spans(&self, spans: Vec<TelemetrySpan>) -> Result<(), TelemetryError>;
    async fn write_metrics(&self, points: Vec<TelemetryMetricPoint>) -> Result<(), TelemetryError>;
    async fn query_traces(
        &self,
        filter: TelemetryTraceFilter,
        offset: usize,
        limit: usize,
    ) -> Result<TelemetryPage<TelemetryTrace>, TelemetryError>;
    /// Bounded batch read. Ordering is trace id, start timestamp, span id; missing traces contribute no rows.
    async fn query_spans(
        &self,
        trace_ids: Vec<String>,
        offset: usize,
        limit: usize,
    ) -> Result<TelemetryPage<TelemetrySpan>, TelemetryError>;
    /// Aggregate in storage without materializing every trace in the caller.
    async fn aggregate_traces(
        &self,
        filter: TelemetryTraceFilter,
        classifications: BTreeMap<String, TelemetryTracePredicate>,
        buckets: Option<TelemetryBucketQuery>,
    ) -> Result<TelemetryTraceStatistics, TelemetryError>;
    async fn spans(&self, trace_id: String) -> Result<Vec<TelemetrySpan>, TelemetryError>;
    /// Up to 128 attributes in a single query.
    /// Sum requested numeric span attributes; the caller supplies semantic attribute names.
    async fn sum_span_attributes(
        &self,
        filter: TelemetryTraceFilter,
        span_name: String,
        attributes: Vec<String>,
    ) -> Result<BTreeMap<String, u64>, TelemetryError>;
    async fn metric_points(
        &self,
        filter: TelemetryMetricFilter,
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
