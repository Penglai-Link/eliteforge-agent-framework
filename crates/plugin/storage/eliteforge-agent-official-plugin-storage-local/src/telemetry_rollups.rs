//! Incremental list metrics. Placeholders retain child totals when parents arrive later.
use eliteforge_agent_core::TelemetrySpan;
use rusqlite::{Connection, OptionalExtension, params_from_iter, types::Value};
use std::{
    collections::{HashMap, VecDeque},
    sync::OnceLock,
};

const FIELDS: [&str; 7] = [
    "span_count",
    "error_count",
    "model_count",
    "tool_count",
    "input_tokens",
    "cached_input_tokens",
    "output_tokens",
];
type Counters = [i64; 7];

fn own(name: &str, status: i32, attrs: &std::collections::BTreeMap<String, String>) -> Counters {
    let error = attrs
        .get("tron.operation.active")
        .is_none_or(|v| v != "true")
        && match attrs.get("agent.operation.outcome").map(String::as_str) {
            Some(
                "unknown" | "cancelled" | "waiting" | "awaiting_input" | "interrupted"
                | "step_limit" | "tool_call_limit",
            ) => false,
            Some("error") => true,
            _ => status == 2,
        };
    let model = name == "agent.model";
    let token = |key: &str| {
        if model {
            attrs
                .get(key)
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0)
                .min(i64::MAX as u64) as i64
        } else {
            0
        }
    };
    [
        i64::from(name != "tron.session.run"),
        i64::from(name != "tron.session.run" && error),
        i64::from(model),
        i64::from(name == "agent.tool"),
        token("gen_ai.usage.input_tokens"),
        token("gen_ai.usage.cached_input_tokens"),
        token("gen_ai.usage.output_tokens"),
    ]
}
fn columns() -> String {
    FIELDS.join(",")
}
fn own_columns() -> String {
    FIELDS.map(|f| format!("own_{f}")).join(",")
}
fn read(row: &rusqlite::Row<'_>, start: usize) -> rusqlite::Result<Counters> {
    let mut result = [0; 7];
    for (i, value) in result.iter_mut().enumerate() {
        *value = row.get(start + i)?;
    }
    Ok(result)
}
fn params(trace: &str, id: &str, values: Counters) -> Vec<Value> {
    let mut result = vec![Value::Text(trace.into()), Value::Text(id.into())];
    result.extend(values.into_iter().map(Value::Integer));
    result
}
struct Statements {
    trace: String,
    ancestors: String,
    previous: String,
    upsert: String,
}
fn statements() -> &'static Statements {
    static SQL: OnceLock<Statements> = OnceLock::new();
    SQL.get_or_init(|| {
        let columns=columns();
        let own_columns=own_columns();
        let increments=FIELDS.map(|f|format!("{f}={f}+excluded.{f}")).join(",");
        let updates=FIELDS.map(|f|format!("own_{f}=excluded.own_{f},{f}=excluded.{f}")).join(",");
        Statements {
            trace:format!("INSERT INTO ef_telemetry_trace_rollups(trace_id,{columns}) VALUES(?,?,?,?,?,?,?,?) ON CONFLICT(trace_id) DO UPDATE SET {increments}"),
            ancestors:format!("WITH RECURSIVE ancestors(id,parent_id) AS (
                SELECT ?2,(SELECT parent_id FROM ef_telemetry_span_rollups WHERE trace_id=?1 AND span_id=?2)
                UNION SELECT r.span_id,r.parent_id FROM ef_telemetry_span_rollups r JOIN ancestors a ON r.span_id=a.parent_id WHERE r.trace_id=?1 AND r.span_id<>?10
            ) INSERT INTO ef_telemetry_span_rollups(trace_id,span_id,{columns}) SELECT ?1,id,?3,?4,?5,?6,?7,?8,?9 FROM ancestors WHERE id<>?10
              ON CONFLICT(trace_id,span_id) DO UPDATE SET {increments}"),
            previous:format!("SELECT parent_id,{own_columns},{columns} FROM ef_telemetry_span_rollups WHERE trace_id=?1 AND span_id=?2"),
            upsert:format!("INSERT INTO ef_telemetry_span_rollups(trace_id,span_id,parent_id,{own_columns},{columns}) VALUES({}) ON CONFLICT(trace_id,span_id) DO UPDATE SET parent_id=excluded.parent_id,{updates}",vec!["?";17].join(",")),
        }
    })
}
fn add_trace(c: &Connection, trace: &str, delta: Counters) -> rusqlite::Result<()> {
    if delta == [0; 7] {
        return Ok(());
    }
    let mut values = vec![Value::Text(trace.into())];
    values.extend(delta.into_iter().map(Value::Integer));
    c.prepare_cached(&statements().trace)?
        .execute(params_from_iter(values))?;
    Ok(())
}
fn propagate(
    c: &Connection,
    trace: &str,
    child: &str,
    parent: Option<&str>,
    delta: Counters,
) -> rusqlite::Result<()> {
    let Some(parent) = parent else {
        return Ok(());
    };
    if delta == [0; 7] || parent == child {
        return Ok(());
    }
    let mut values = params(trace, parent, delta);
    values.push(Value::Text(child.into()));
    // UNION prevents cycles; the changed node is already updated separately.
    c.prepare_cached(&statements().ancestors)?
        .execute(params_from_iter(values))?;
    Ok(())
}
pub(super) fn write(c: &Connection, span: &TelemetrySpan) -> rusqlite::Result<()> {
    let previous = c
        .prepare_cached(&statements().previous)?
        .query_row(rusqlite::params![span.trace_id, span.span_id], |r| {
            Ok((r.get::<_, Option<String>>(0)?, read(r, 1)?, read(r, 8)?))
        })
        .optional()?;
    let exists = previous.is_some();
    let (old_parent, old_own, before) = previous.unwrap_or((None, [0; 7], [0; 7]));
    let next = own(&span.name, span.status_code, &span.attributes);
    let delta = std::array::from_fn(|i| next[i] - old_own[i]);
    if exists && delta == [0; 7] && old_parent == span.parent_span_id {
        return Ok(());
    }
    let after = std::array::from_fn(|i| before[i] + delta[i]);
    let mut values = vec![
        Value::Text(span.trace_id.clone()),
        Value::Text(span.span_id.clone()),
        span.parent_span_id
            .clone()
            .map(Value::Text)
            .unwrap_or(Value::Null),
    ];
    values.extend(next.into_iter().chain(after).map(Value::Integer));
    c.prepare_cached(&statements().upsert)?
        .execute(params_from_iter(values))?;
    if old_parent == span.parent_span_id {
        propagate(
            c,
            &span.trace_id,
            &span.span_id,
            span.parent_span_id.as_deref(),
            delta,
        )?;
    } else {
        propagate(
            c,
            &span.trace_id,
            &span.span_id,
            old_parent.as_deref(),
            before.map(|v| -v),
        )?;
        propagate(
            c,
            &span.trace_id,
            &span.span_id,
            span.parent_span_id.as_deref(),
            after,
        )?;
    }
    add_trace(c, &span.trace_id, delta)
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Seed {
    trace_id: String,
    span_id: String,
    parent_span_id: Option<String>,
    name: String,
    status_code: i32,
    attributes: std::collections::BTreeMap<String, String>,
}
struct Node {
    parent: Option<String>,
    own: Counters,
    total: Counters,
    children: usize,
}

/// One-time backfill uses a leaf-to-root pass, avoiding one ancestry query per old span.
pub(super) fn backfill(c: &mut Connection) -> rusqlite::Result<()> {
    if c.query_row(
        "SELECT EXISTS(SELECT 1 FROM ef_telemetry_span_rollups)",
        [],
        |r| r.get::<_, bool>(0),
    )? {
        return Ok(());
    }
    let tx = c.transaction()?;
    let mut nodes = HashMap::<(String, String), Node>::new();
    {
        let mut stmt = tx.prepare("SELECT data FROM ef_telemetry_spans")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let payload: String = row.get(0)?;
            let seed: Seed =
                serde_json::from_str(&payload).map_err(|_| rusqlite::Error::InvalidQuery)?;
            let values = own(&seed.name, seed.status_code, &seed.attributes);
            nodes.insert(
                (seed.trace_id, seed.span_id),
                Node {
                    parent: seed.parent_span_id,
                    own: values,
                    total: values,
                    children: 0,
                },
            );
        }
    }
    let links = nodes
        .iter()
        .filter_map(|((trace, id), node)| {
            node.parent
                .as_ref()
                .filter(|p| *p != id)
                .map(|p| (trace.clone(), p.clone()))
        })
        .collect::<Vec<_>>();
    for key in links {
        nodes
            .entry(key)
            .or_insert(Node {
                parent: None,
                own: [0; 7],
                total: [0; 7],
                children: 0,
            })
            .children += 1;
    }
    let mut leaves = nodes
        .iter()
        .filter(|(_, node)| node.children == 0)
        .map(|(key, _)| key.clone())
        .collect::<VecDeque<_>>();
    while let Some(key) = leaves.pop_front() {
        let child = &nodes[&key];
        let total = child.total;
        if let Some(parent) = child.parent.clone().filter(|p| p != &key.1) {
            let parent_key = (key.0, parent);
            if let Some(node) = nodes.get_mut(&parent_key) {
                for i in 0..7 {
                    node.total[i] += total[i];
                }
                node.children -= 1;
                if node.children == 0 {
                    leaves.push_back(parent_key);
                }
            }
        }
    }
    let mut traces = HashMap::<String, Counters>::new();
    {
        let sql = format!(
            "INSERT INTO ef_telemetry_span_rollups(trace_id,span_id,parent_id,{},{}) VALUES({})",
            own_columns(),
            columns(),
            vec!["?"; 17].join(",")
        );
        let mut insert = tx.prepare(&sql)?;
        for ((trace, id), node) in nodes {
            let trace_total = traces.entry(trace.clone()).or_insert([0; 7]);
            for i in 0..7 {
                trace_total[i] += node.own[i];
            }
            let mut values = vec![
                Value::Text(trace),
                Value::Text(id),
                node.parent.map(Value::Text).unwrap_or(Value::Null),
            ];
            values.extend(node.own.into_iter().chain(node.total).map(Value::Integer));
            insert.execute(params_from_iter(values))?;
        }
    }
    for (trace, total) in traces {
        add_trace(&tx, &trace, total)?;
    }
    tx.commit()
}
