use super::*;
use rusqlite::{OptionalExtension, params_from_iter, types::Value};
use std::collections::BTreeMap;

pub(super) fn bind(values: &mut Vec<Value>, value: impl Into<Value>) -> String {
    values.push(value.into());
    format!("?{}", values.len())
}
fn json_path(prefix: &str, key: &str) -> String {
    format!("{prefix}.{}", serde_json::to_string(key).unwrap())
}
pub(super) fn trace_filter(f: TelemetryTraceFilter, values: &mut Vec<Value>) -> String {
    let mut sql = "1=1".to_owned();
    if let Some(id) = f.trace_id {
        sql.push_str(&format!(" AND t.trace_id={}", bind(values, id)));
    }
    for (key, value) in f.attributes {
        let key = bind(values, key);
        let value = bind(values, value);
        sql.push_str(&format!(" AND t.trace_id IN (SELECT trace_id FROM ef_telemetry_trace_attributes WHERE attribute={key} AND value={value})"));
    }
    sql
}
pub(super) fn valid(limit: usize) -> Result<(), TelemetryError> {
    if !(1..=50_000).contains(&limit) {
        return Err(TelemetryError::Invalid(
            "page limit must be 1..50000".into(),
        ));
    }
    Ok(())
}
pub(super) fn snapshot(
    c: &mut Connection,
) -> rusqlite::Result<crate::database::RequiredTransaction<'_>> {
    Ok(if c.is_autocommit() {
        crate::database::RequiredTransaction::Owned(
            c.transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)?,
        )
    } else {
        crate::database::RequiredTransaction::Joined(c)
    })
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
    let mut args = Vec::new();
    let filter = trace_filter(f, &mut args);
    let offset = i64::try_from(offset)
        .map_err(|_| TelemetryError::Invalid("offset exceeds storage range".into()))?;
    store.database.call_read(move |c| ->rusqlite::Result<_> {
        let tx=snapshot(c)?;
        let total=tx.query_row(&format!("SELECT count(*) FROM ef_telemetry_traces t WHERE {filter}"),params_from_iter(&args),|r|r.get(0))?;
        let limit=bind(&mut args,limit as i64);let offset=bind(&mut args,offset);
        let records={let mut q=tx.prepare(&format!("SELECT trace_id,started_at,ended_at,status_code,attributes,abandoned FROM ef_telemetry_traces t WHERE {filter} ORDER BY started_at DESC,trace_id DESC LIMIT {limit} OFFSET {offset}"))?;
            q.query_map(params_from_iter(args),|r|Ok(TelemetryTrace {trace_id:r.get(0)?,started_at:r.get(1)?,ended_at:r.get(2)?,status_code:r.get(3)?,attributes:decode_json(r,4)?,abandoned:r.get(5)?}))?.collect::<rusqlite::Result<_>>()?};
        tx.commit()?;Ok(TelemetryPage{total,records})
    }).await.map_err(error)
}
pub(super) async fn spans(
    store: &SqliteTelemetryStore,
    id: String,
) -> Result<Vec<TelemetrySpan>, TelemetryError> {
    store.database.call_read(move |c| ->rusqlite::Result<_> {
        let mut q=c.prepare("SELECT data FROM ef_telemetry_spans WHERE trace_id=?1 ORDER BY json_extract(data,'$.startedAt'),span_id")?;
        q.query_map([id],|r|decode_json(r,0))?.collect()
    }).await.map_err(error)
}
pub(super) async fn span_page(
    store: &SqliteTelemetryStore,
    ids: Vec<String>,
    offset: usize,
    limit: usize,
) -> Result<TelemetryPage<TelemetrySpan>, TelemetryError> {
    valid(limit)?;
    let ids = serde_json::to_string(&ids).map_err(error)?;
    let offset = i64::try_from(offset)
        .map_err(|_| TelemetryError::Invalid("offset exceeds storage range".into()))?;
    store.database.call_read(move |c| ->rusqlite::Result<_> {
        let tx=snapshot(c)?;
        let total=tx.query_row("SELECT count(*) FROM ef_telemetry_spans WHERE trace_id IN (SELECT value FROM json_each(?1))",[&ids],|r|r.get(0))?;
        let records={let mut q=tx.prepare("SELECT data FROM ef_telemetry_spans WHERE trace_id IN (SELECT value FROM json_each(?1)) ORDER BY trace_id,json_extract(data,'$.startedAt'),span_id LIMIT ?2 OFFSET ?3")?;
            q.query_map(params![ids,limit,offset],|r|decode_json(r,0))?.collect::<rusqlite::Result<_>>()?};
        tx.commit()?;Ok(TelemetryPage{total,records})
    }).await.map_err(error)
}
pub(super) async fn sums(
    store: &SqliteTelemetryStore,
    f: TelemetryTraceFilter,
    name: String,
    attrs: Vec<String>,
) -> Result<BTreeMap<String, u64>, TelemetryError> {
    if attrs.len() > 128 {
        return Err(TelemetryError::Invalid(
            "at most 128 aggregate attributes".into(),
        ));
    }
    if attrs.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut args = Vec::new();
    let filter = trace_filter(f, &mut args);
    let name = bind(&mut args, name);
    let requested = bind(&mut args, serde_json::to_string(&attrs).map_err(error)?);
    store.database.call_read(move |c| ->rusqlite::Result<_> {
        let sql=format!("SELECT a.attribute,SUM(a.value) FROM ef_telemetry_span_attributes a WHERE a.name={name} AND a.attribute IN (SELECT value FROM json_each({requested})) AND a.trace_id IN (SELECT t.trace_id FROM ef_telemetry_traces t WHERE {filter}) GROUP BY a.attribute");
        let mut out=attrs.into_iter().map(|key|(key,0)).collect::<BTreeMap<_,_>>();
        let mut q=c.prepare(&sql)?;
        let rows=q.query_map(params_from_iter(args),|r|Ok((r.get::<_,String>(0)?,r.get::<_,f64>(1)?.max(0.) as u64)))?;
        for row in rows {let (key,value)=row?;out.insert(key,value);}
        Ok(out)
    }).await.map_err(error)
}

fn metric_selection(f: TelemetryMetricFilter, args: &mut Vec<Value>) -> (String, String) {
    let mut attrs = f.attributes.into_iter();
    let (source, mut sql) = if let Some((key, value)) = attrs.next() {
        let key = bind(args, key);
        let value = bind(args, value);
        (
            "ef_telemetry_metric_attributes p".to_owned(),
            format!("p.attribute={key} AND p.value={value}"),
        )
    } else {
        ("ef_telemetry_metric_points p".to_owned(), "1=1".into())
    };
    if let Some(name) = f.name {
        sql.push_str(&format!(" AND p.name={}", bind(args, name)));
    }
    for (key, value) in attrs {
        let key = bind(args, key);
        let value = bind(args, value);
        sql.push_str(&format!(" AND p.point_id IN (SELECT point_id FROM ef_telemetry_metric_attributes WHERE attribute={key} AND value={value})"));
    }
    (source, sql)
}
pub(super) async fn points(
    store: &SqliteTelemetryStore,
    f: TelemetryMetricFilter,
    offset: usize,
    limit: usize,
) -> Result<TelemetryPage<TelemetryMetricPoint>, TelemetryError> {
    valid(limit)?;
    let mut args = Vec::new();
    let (source, filter) = metric_selection(f, &mut args);
    let offset = i64::try_from(offset)
        .map_err(|_| TelemetryError::Invalid("offset exceeds storage range".into()))?;
    store.database.call_read(move |c| ->rusqlite::Result<_> {
        let tx=snapshot(c)?;
        let total=tx.query_row(&format!("SELECT count(*) FROM {source} WHERE {filter}"),params_from_iter(&args),|r|r.get(0))?;
        let limit=bind(&mut args,limit as i64);let offset=bind(&mut args,offset);
        // Select ordered IDs from a covering attribute/time index before loading JSON.
        let records={let mut q=tx.prepare(&format!("WITH page AS MATERIALIZED (SELECT p.point_id FROM {source} WHERE {filter} ORDER BY p.timestamp DESC,p.point_id DESC LIMIT {limit} OFFSET {offset}) SELECT p.data FROM page JOIN ef_telemetry_metric_points p ON p.point_id=page.point_id ORDER BY p.timestamp DESC,p.point_id DESC"))?;
            q.query_map(params_from_iter(args),|r|decode_json(r,0))?.collect::<rusqlite::Result<_>>()?};
        tx.commit()?;Ok(TelemetryPage{total,records})
    }).await.map_err(error)
}
pub(super) async fn count_points_by_attribute(
    store: &SqliteTelemetryStore,
    attribute: String,
    value: String,
) -> Result<usize, TelemetryError> {
    store.database.call_read(move |c|c.query_row("SELECT count(*) FROM ef_telemetry_metric_attributes WHERE attribute=?1 AND value=?2",params![attribute,value],|r|r.get(0))).await.map_err(error)
}
pub(super) async fn latest(
    store: &SqliteTelemetryStore,
    name: String,
    resource: BTreeMap<String, String>,
) -> Result<Option<TelemetryMetricPoint>, TelemetryError> {
    let mut args = Vec::new();
    let mut filter = format!("name={}", bind(&mut args, name));
    for (key, value) in resource {
        let path = bind(&mut args, json_path("$.resource", &key));
        let value = bind(&mut args, value);
        filter.push_str(&format!(" AND json_extract(data,{path})={value}"));
    }
    store.database.call_read(move |c|c.query_row(&format!("SELECT data FROM ef_telemetry_metric_points WHERE {filter} ORDER BY timestamp DESC,point_id DESC LIMIT 1"),params_from_iter(args),|r|decode_json(r,0)).optional()).await.map_err(error)
}
