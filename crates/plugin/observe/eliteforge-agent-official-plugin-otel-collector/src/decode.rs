use eliteforge_agent_core::{
    TelemetryMetricPoint, TelemetrySpan, TelemetrySpanEvent, TelemetrySpanLink,
};
use opentelemetry_proto::tonic::{
    collector::{metrics::v1::ExportMetricsServiceRequest, trace::v1::ExportTraceServiceRequest},
    common::v1::{KeyValue, any_value::Value},
    metrics::v1::metric::Data,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
fn attrs(values: &[KeyValue]) -> BTreeMap<String, String> {
    values
        .iter()
        .filter_map(|kv| {
            let value = kv.value.as_ref()?.value.as_ref()?;
            let text = match value {
                Value::StringValue(s) => s.clone(),
                Value::IntValue(n) => n.to_string(),
                Value::DoubleValue(n) => n.to_string(),
                Value::BoolValue(b) => b.to_string(),
                other => serde_json::to_string(other).ok()?,
            };
            Some((kv.key.clone(), text))
        })
        .collect()
}
fn valid_id(id: &[u8], length: usize) -> bool {
    id.len() == length && id.iter().any(|b| *b != 0)
}
pub(super) fn spans(request: ExportTraceServiceRequest) -> Result<Vec<TelemetrySpan>, ()> {
    let mut records = Vec::new();
    for resource in request.resource_spans {
        let resource_attrs = attrs(&resource.resource.clone().unwrap_or_default().attributes);
        for scope in resource.scope_spans {
            for s in scope.spans {
                if !valid_id(&s.trace_id, 16)
                    || !valid_id(&s.span_id, 8)
                    || (!s.parent_span_id.is_empty() && !valid_id(&s.parent_span_id, 8))
                    || s.start_time_unix_nano == 0
                    || s.end_time_unix_nano < s.start_time_unix_nano
                {
                    return Err(());
                }
                let mut attributes = resource_attrs.clone();
                attributes.extend(attrs(&s.attributes));
                let data = serde_json::json!({"durationMs":(s.end_time_unix_nano-s.start_time_unix_nano) as f64/1_000_000.,"span":s,"resource":resource.resource,"scope":scope.scope,"schemaUrl":scope.schema_url,"resourceSchemaUrl":resource.schema_url});
                records.push(TelemetrySpan {
                    trace_id: hex::encode(&s.trace_id),
                    span_id: hex::encode(&s.span_id),
                    parent_span_id: (!s.parent_span_id.is_empty())
                        .then(|| hex::encode(&s.parent_span_id)),
                    name: s.name,
                    started_at: (s.start_time_unix_nano / 1_000_000) as i64,
                    ended_at: (s.end_time_unix_nano / 1_000_000) as i64,
                    status_code: s.status.map(|s| s.code).unwrap_or(0),
                    attributes,
                    data,
                    events: s
                        .events
                        .into_iter()
                        .map(|e| TelemetrySpanEvent {
                            name: e.name,
                            timestamp: (e.time_unix_nano / 1_000_000) as i64,
                            attributes: attrs(&e.attributes),
                        })
                        .collect(),
                    links: s
                        .links
                        .into_iter()
                        .map(|l| TelemetrySpanLink {
                            trace_id: hex::encode(l.trace_id),
                            span_id: hex::encode(l.span_id),
                        })
                        .collect(),
                });
                if records.len() > 50_000 {
                    return Err(());
                }
            }
        }
    }
    Ok(records)
}
pub(super) fn metrics(
    request: ExportMetricsServiceRequest,
) -> Result<Vec<TelemetryMetricPoint>, ()> {
    let mut points = Vec::new();
    for resource in request.resource_metrics {
        let resource_attrs = attrs(&resource.resource.unwrap_or_default().attributes);
        for scope in resource.scope_metrics {
            let scope_json=serde_json::json!({"scope":scope.scope,"schemaUrl":scope.schema_url,"resourceSchemaUrl":resource.schema_url}).to_string();
            for metric in scope.metrics {
                let Some(data) = metric.data else {
                    return Err(());
                };
                let (kind, temporality, monotonic, values): (
                    &str,
                    i32,
                    bool,
                    Vec<(u64, serde_json::Value)>,
                ) = match data {
                    Data::Gauge(g) => (
                        "Gauge",
                        0,
                        false,
                        g.data_points
                            .into_iter()
                            .map(|p| (p.time_unix_nano, serde_json::to_value(p).unwrap()))
                            .collect(),
                    ),
                    Data::Sum(g) => (
                        "Sum",
                        g.aggregation_temporality,
                        g.is_monotonic,
                        g.data_points
                            .into_iter()
                            .map(|p| (p.time_unix_nano, serde_json::to_value(p).unwrap()))
                            .collect(),
                    ),
                    Data::Histogram(g) => (
                        "Histogram",
                        g.aggregation_temporality,
                        false,
                        g.data_points
                            .into_iter()
                            .map(|p| (p.time_unix_nano, serde_json::to_value(p).unwrap()))
                            .collect(),
                    ),
                    Data::ExponentialHistogram(g) => (
                        "ExponentialHistogram",
                        g.aggregation_temporality,
                        false,
                        g.data_points
                            .into_iter()
                            .map(|p| (p.time_unix_nano, serde_json::to_value(p).unwrap()))
                            .collect(),
                    ),
                    Data::Summary(g) => (
                        "Summary",
                        0,
                        false,
                        g.data_points
                            .into_iter()
                            .map(|p| (p.time_unix_nano, serde_json::to_value(p).unwrap()))
                            .collect(),
                    ),
                };
                for (timestamp, mut data) in values {
                    normalize_ints(&mut data);
                    if timestamp == 0 {
                        return Err(());
                    }
                    let identity = serde_json::json!([
                        resource_attrs,
                        scope_json,
                        metric.name,
                        metric.unit,
                        kind,
                        temporality,
                        monotonic,
                        data
                    ])
                    .to_string();
                    points.push(TelemetryMetricPoint {
                        point_id: hex::encode(Sha256::digest(identity.as_bytes())),
                        name: metric.name.clone(),
                        description: metric.description.clone(),
                        unit: metric.unit.clone(),
                        kind: kind.into(),
                        temporality,
                        monotonic,
                        timestamp: (timestamp / 1_000_000) as i64,
                        resource: resource_attrs.clone(),
                        scope: scope_json.clone(),
                        data,
                    });
                    if points.len() > 50_000 {
                        return Err(());
                    }
                }
            }
        }
    }
    Ok(points)
}

// Preserve signed 64-bit point/exemplar values across the browser JSON boundary.
fn normalize_ints(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                if key == "asInt" && value.is_i64() {
                    *value = serde_json::Value::String(value.to_string());
                } else {
                    normalize_ints(value);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                normalize_ints(value);
            }
        }
        _ => {}
    }
}
