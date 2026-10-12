use super::*;
use rusqlite::{OptionalExtension, params_from_iter, types::Value};
use std::collections::BTreeMap;

fn json_path(prefix: &str, key: &str) -> String {
    format!("{prefix}.{}", serde_json::to_string(key).unwrap())
}

fn filter(f: TelemetryTraceFilter) -> (String, Vec<Value>) {
    let mut sql = "1=1".to_owned();
    let mut values = Vec::new();
    if let Some(id) = f.trace_id {
        sql.push_str(" AND t.trace_id=?");
        values.push(Value::Text(id));
    }
    for (key, value) in f.attributes {
        if key == "tron.workspace.code" {
            sql.push_str(" AND json_extract(t.attributes,'$.\"tron.workspace.code\"')=?");
        } else {
            sql.push_str(" AND json_extract(t.attributes,?)=?");
            values.push(Value::Text(json_path("$", &key)));
        }
        values.push(Value::Text(value));
    }
    (sql, values)
}

fn valid(limit: usize) -> Result<(), TelemetryError> {
    if limit == 0 || limit > 50_000 {
        return Err(TelemetryError::Invalid(
            "page limit must be 1..50000".into(),
        ));
    }
    Ok(())
}

fn decode_json<T: serde::de::DeserializeOwned>(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> rusqlite::Result<T> {
    let json: String = row.get(index)?;
    serde_json::from_str(&json).map_err(|_| rusqlite::Error::InvalidQuery)
}

pub(super) async fn traces(
    store: &SqliteTelemetryStore,
    f: TelemetryTraceFilter,
    offset: usize,
    limit: usize,
) -> Result<TelemetryPage<TelemetryTrace>, TelemetryError> {
    valid(limit)?;
    let (sql, mut values) = filter(f);
    store.database.call(move |c| -> rusqlite::Result<_> {
        let total = c.query_row(
            &format!("SELECT COUNT(*) FROM ef_telemetry_traces t WHERE {sql}"),
            params_from_iter(&values), |r| r.get(0),
        )?;
        values.push(Value::Integer(limit as i64));
        values.push(Value::Integer(offset.try_into().map_err(|_| rusqlite::Error::InvalidQuery)?));
        let mut stmt = c.prepare(&format!(
            "SELECT trace_id,started_at,ended_at,status_code,attributes,abandoned FROM ef_telemetry_traces t WHERE {sql} ORDER BY started_at DESC,trace_id DESC LIMIT ? OFFSET ?"
        ))?;
        let records = stmt.query_map(params_from_iter(&values), |r| {
            Ok(TelemetryTrace {
                trace_id:r.get(0)?,started_at:r.get(1)?,ended_at:r.get(2)?,status_code:r.get(3)?,
                attributes:decode_json(r,4)?,abandoned:r.get(5)?,
            })
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(TelemetryPage { total, records })
    }).await.map_err(error)
}

pub(super) async fn spans(
    store: &SqliteTelemetryStore,
    id: String,
) -> Result<Vec<TelemetrySpan>, TelemetryError> {
    store.database.call(move |c| -> rusqlite::Result<_> {
        let mut stmt = c.prepare(
            "SELECT data FROM ef_telemetry_spans WHERE trace_id=?1 ORDER BY json_extract(data,'$.startedAt'),span_id"
        )?;
        stmt.query_map([id], |r| decode_json(r,0))?.collect()
    }).await.map_err(error)
}

pub(super) async fn sums(
    store: &SqliteTelemetryStore,
    f: TelemetryTraceFilter,
    name: String,
    attrs: Vec<String>,
) -> Result<BTreeMap<String, u64>, TelemetryError> {
    let (filter, mut values) = filter(f);
    values.push(Value::Text(name));
    store.database.call(move |c| -> rusqlite::Result<_> {
        let mut out = BTreeMap::new();
        let sql = format!(
            "SELECT COALESCE(SUM(CAST(json_extract(s.data,?) AS REAL)),0) FROM ef_telemetry_spans s JOIN ef_telemetry_traces t ON t.trace_id=s.trace_id WHERE {filter} AND json_extract(s.data,'$.name')=?"
        );
        let mut stmt = c.prepare(&sql)?;
        for key in attrs {
            let mut args = vec![Value::Text(json_path("$.attributes", &key))];
            args.extend(values.clone());
            let sum:f64 = stmt.query_row(params_from_iter(args), |r| r.get(0))?;
            out.insert(key, sum.max(0.) as u64);
        }
        Ok(out)
    }).await.map_err(error)
}

pub(super) async fn points(
    store: &SqliteTelemetryStore,
    name: Option<String>,
    offset: usize,
    limit: usize,
) -> Result<TelemetryPage<TelemetryMetricPoint>, TelemetryError> {
    valid(limit)?;
    store.database.call(move |c| -> rusqlite::Result<_> {
        let total = c.query_row(
            "SELECT COUNT(*) FROM ef_telemetry_metric_points WHERE (?1 IS NULL OR name=?1)",
            [&name], |r| r.get(0),
        )?;
        let mut stmt = c.prepare(
            "SELECT data FROM ef_telemetry_metric_points WHERE (?1 IS NULL OR name=?1) ORDER BY timestamp DESC,point_id DESC LIMIT ?2 OFFSET ?3"
        )?;
        let records = stmt.query_map(params![name,limit,offset], |r| decode_json(r,0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(TelemetryPage { total, records })
    }).await.map_err(error)
}

pub(super) async fn count_points_by_attribute(
    store: &SqliteTelemetryStore,
    attribute: String,
    value: String,
) -> Result<usize, TelemetryError> {
    let resource_path = json_path("$.resource", &attribute);
    store
        .database
        .call(move |c| {
            c.query_row(
                "SELECT COUNT(*) FROM ef_telemetry_metric_points p WHERE json_extract(p.data,?1)=?2 OR EXISTS (SELECT 1 FROM json_each(json_extract(p.data,'$.data.attributes')) a WHERE json_extract(a.value,'$.key')=?3 AND json_extract(a.value,'$.value.stringValue')=?2)",
                rusqlite::params![resource_path, value, attribute],
                |row| row.get(0),
            )
        })
        .await
        .map_err(error)
}

pub(super) async fn latest(
    store: &SqliteTelemetryStore,
    name: String,
    resource: BTreeMap<String, String>,
) -> Result<Option<TelemetryMetricPoint>, TelemetryError> {
    let mut sql = "name=?".to_owned();
    let mut args = vec![Value::Text(name)];
    for (key, value) in resource {
        sql.push_str(" AND json_extract(data,?)=?");
        args.push(Value::Text(json_path("$.resource", &key)));
        args.push(Value::Text(value));
    }
    store.database.call(move |c| -> rusqlite::Result<_> {
        c.query_row(
            &format!("SELECT data FROM ef_telemetry_metric_points WHERE {sql} ORDER BY timestamp DESC,point_id DESC LIMIT 1"),
            params_from_iter(args), |r| decode_json(r,0),
        ).optional()
    }).await.map_err(error)
}
