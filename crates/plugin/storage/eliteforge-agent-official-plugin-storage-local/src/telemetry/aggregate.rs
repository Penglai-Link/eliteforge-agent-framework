//! Host-neutral classification and exact duration aggregates in one read snapshot.
use super::query::{bind, snapshot, trace_filter};
use super::*;
use rusqlite::{params_from_iter, types::Value};
use std::collections::BTreeMap;

fn predicate(
    p: TelemetryTracePredicate,
    args: &mut Vec<Value>,
    depth: usize,
) -> Result<String, TelemetryError> {
    if depth > 16 || args.len() > 512 {
        return Err(TelemetryError::Invalid(
            "classification is too complex".into(),
        ));
    }
    Ok(match p {
        TelemetryTracePredicate::Attribute { key, value } => {
            let path = bind(
                args,
                format!("$.{}", serde_json::to_string(&key).map_err(error)?),
            );
            let value = bind(args, value);
            format!("COALESCE(json_extract(t.attributes,{path})={value},0)")
        }
        TelemetryTracePredicate::StatusCode(code) => format!("t.status_code={}", bind(args, code)),
        TelemetryTracePredicate::HasEnded(true) => "t.ended_at IS NOT NULL".into(),
        TelemetryTracePredicate::HasEnded(false) => "t.ended_at IS NULL".into(),
        TelemetryTracePredicate::Abandoned(value) => format!("t.abandoned={}", i32::from(value)),
        TelemetryTracePredicate::Not(p) => format!("NOT ({})", predicate(*p, args, depth + 1)?),
        TelemetryTracePredicate::All(parts) => {
            if parts.is_empty() {
                "1".into()
            } else {
                parts
                    .into_iter()
                    .map(|p| predicate(p, args, depth + 1).map(|p| format!("({p})")))
                    .collect::<Result<Vec<_>, _>>()?
                    .join(" AND ")
            }
        }
        TelemetryTracePredicate::Any(parts) => {
            if parts.is_empty() {
                "0".into()
            } else {
                parts
                    .into_iter()
                    .map(|p| predicate(p, args, depth + 1).map(|p| format!("({p})")))
                    .collect::<Result<Vec<_>, _>>()?
                    .join(" OR ")
            }
        }
    })
}
pub(super) async fn query(
    store: &SqliteTelemetryStore,
    filter: TelemetryTraceFilter,
    classifications: BTreeMap<String, TelemetryTracePredicate>,
    buckets: Option<TelemetryBucketQuery>,
) -> Result<TelemetryTraceStatistics, TelemetryError> {
    if classifications.len() > 32 {
        return Err(TelemetryError::Invalid("at most 32 classifications".into()));
    }
    if buckets.as_ref().is_some_and(|b| {
        b.width_ms <= 0
            || b.end
                .checked_sub(b.start)
                .is_none_or(|range| range < 0 || range / b.width_ms > 1000)
    }) {
        return Err(TelemetryError::Invalid("invalid bucket range".into()));
    }
    let mut args = Vec::new();
    let filter = trace_filter(filter, &mut args);
    let mut names = Vec::new();
    let mut columns = Vec::new();
    for (i, (name, p)) in classifications.into_iter().enumerate() {
        names.push(name);
        columns.push(format!(",({}) AS c{i}", predicate(p, &mut args, 0)?));
    }
    let base = format!(
        "WITH selected AS MATERIALIZED (SELECT started_at,CASE WHEN ended_at IS NOT NULL THEN MAX(0,ended_at-started_at) END AS duration{} FROM ef_telemetry_traces t WHERE {filter})",
        columns.join("")
    );
    let totals = (0..names.len())
        .map(|i| format!(",COALESCE(SUM(c{i}),0)"))
        .collect::<String>();
    let sql = format!(
        "{base}, ranked AS (SELECT duration,ROW_NUMBER() OVER(ORDER BY duration) AS rn,COUNT(*) OVER() AS n FROM selected WHERE duration IS NOT NULL) SELECT COUNT(*),COALESCE(AVG(duration),0),COALESCE((SELECT duration FROM ranked WHERE rn=(n*95+99)/100),0){totals} FROM selected"
    );
    store.database.call_read(move |c| ->rusqlite::Result<_> {
        let tx=snapshot(c)?;
        let mut result=tx.query_row(&sql,params_from_iter(&args),|r|Ok(TelemetryTraceStatistics {
            total:r.get(0)?,average_duration_ms:r.get(1)?,p95_duration_ms:r.get(2)?,
            classified:names.iter().enumerate().map(|(i,name)|Ok((name.clone(),r.get(i+3)?))).collect::<rusqlite::Result<_>>()?,buckets:Vec::new()
        }))?;
        if let Some(b)=buckets {
            let start=bind(&mut args,b.start);let end=bind(&mut args,b.end);let width=bind(&mut args,b.width_ms);
            let sql=format!("{base} SELECT {start}+((started_at-{start})/{width})*{width} AS bucket,COUNT(*){totals} FROM selected WHERE started_at>={start} AND started_at<={end} GROUP BY bucket ORDER BY bucket");
            let mut q=tx.prepare(&sql)?;
            result.buckets=q.query_map(params_from_iter(args),|r|Ok(TelemetryTraceBucket {started_at:r.get(0)?,total:r.get(1)?,classified:names.iter().enumerate().map(|(i,name)|Ok((name.clone(),r.get(i+2)?))).collect::<rusqlite::Result<_>>()?}))?.collect::<rusqlite::Result<_>>()?;
        }
        tx.commit()?;Ok(result)
    }).await.map_err(error)
}
