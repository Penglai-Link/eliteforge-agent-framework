//! Dedicated telemetry storage plugin using the shared SQLite runtime.
mod aggregate;
mod query;
#[path = "telemetry_rollups.rs"]
mod rollups;
use crate::SqliteDatabase;
use async_trait::async_trait;
use eliteforge_agent_core::*;
use rusqlite::{Connection, params};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
#[derive(Clone)]
pub struct SqliteTelemetryStore {
    pub(super) database: SqliteDatabase,
    pub(super) retention: TelemetryRetention,
}
fn error(e: impl std::fmt::Display) -> TelemetryError {
    TelemetryError::Storage(e.to_string())
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
impl SqliteTelemetryStore {
    pub async fn open(
        path: impl Into<PathBuf>,
        retention: TelemetryRetention,
    ) -> Result<Self, TelemetryError> {
        retention.validate()?;
        let path = path.into();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(error)?;
        }
        let database = SqliteDatabase::open(path).await.map_err(error)?;
        Self::with_database(database, retention).await
    }
    pub async fn with_database(
        database: SqliteDatabase,
        retention: TelemetryRetention,
    ) -> Result<Self, TelemetryError> {
        retention.validate()?;
        database.call(|c|->rusqlite::Result<()> {
        let had_span_attributes:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='ef_telemetry_span_attributes')",[],|r|r.get(0))?;
        let had_trace_attributes:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='ef_telemetry_trace_attributes')",[],|r|r.get(0))?;
        let had_attributes:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='ef_telemetry_metric_attributes')",[],|r|r.get(0))?;
        c.execute_batch("CREATE TABLE IF NOT EXISTS ef_telemetry_traces (trace_id TEXT PRIMARY KEY,started_at INTEGER NOT NULL,ended_at INTEGER,status_code INTEGER NOT NULL,attributes TEXT NOT NULL,abandoned INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE IF NOT EXISTS ef_telemetry_trace_attributes(trace_id TEXT NOT NULL REFERENCES ef_telemetry_traces(trace_id) ON DELETE CASCADE,attribute TEXT NOT NULL,value TEXT NOT NULL,PRIMARY KEY(attribute,value,trace_id));
        CREATE INDEX IF NOT EXISTS ef_telemetry_trace_attribute_trace ON ef_telemetry_trace_attributes(trace_id);
        CREATE TRIGGER IF NOT EXISTS ef_telemetry_trace_attribute_insert AFTER INSERT ON ef_telemetry_traces BEGIN INSERT OR IGNORE INTO ef_telemetry_trace_attributes SELECT NEW.trace_id,key,value FROM json_each(NEW.attributes) WHERE type='text'; END;
        CREATE TRIGGER IF NOT EXISTS ef_telemetry_trace_attribute_update AFTER UPDATE OF attributes ON ef_telemetry_traces BEGIN DELETE FROM ef_telemetry_trace_attributes WHERE trace_id=NEW.trace_id; INSERT OR IGNORE INTO ef_telemetry_trace_attributes SELECT NEW.trace_id,key,value FROM json_each(NEW.attributes) WHERE type='text'; END;
        CREATE INDEX IF NOT EXISTS ef_telemetry_traces_time ON ef_telemetry_traces(started_at DESC,trace_id DESC);
        CREATE INDEX IF NOT EXISTS ef_telemetry_traces_workspace_time ON ef_telemetry_traces(json_extract(attributes,'$.\"tron.workspace.code\"'),started_at DESC,trace_id DESC);
        CREATE INDEX IF NOT EXISTS ef_telemetry_traces_retention ON ef_telemetry_traces(COALESCE(ended_at,started_at) DESC,trace_id DESC) WHERE ended_at IS NOT NULL;
        CREATE TABLE IF NOT EXISTS ef_telemetry_spans(trace_id TEXT NOT NULL REFERENCES ef_telemetry_traces(trace_id) ON DELETE CASCADE,span_id TEXT NOT NULL,ended_at INTEGER NOT NULL,data TEXT NOT NULL,PRIMARY KEY(trace_id,span_id));
        CREATE TABLE IF NOT EXISTS ef_telemetry_span_attributes(trace_id TEXT NOT NULL,span_id TEXT NOT NULL,name TEXT NOT NULL,attribute TEXT NOT NULL,value REAL NOT NULL,PRIMARY KEY(trace_id,span_id,attribute),FOREIGN KEY(trace_id,span_id) REFERENCES ef_telemetry_spans(trace_id,span_id) ON DELETE CASCADE);
        CREATE INDEX IF NOT EXISTS ef_telemetry_span_attribute_sum ON ef_telemetry_span_attributes(name,attribute,trace_id,value);
        CREATE TRIGGER IF NOT EXISTS ef_telemetry_span_attribute_insert AFTER INSERT ON ef_telemetry_spans BEGIN INSERT INTO ef_telemetry_span_attributes SELECT NEW.trace_id,NEW.span_id,json_extract(NEW.data,'$.name'),key,CAST(value AS REAL) FROM json_each(NEW.data,'$.attributes'); END;
        CREATE TRIGGER IF NOT EXISTS ef_telemetry_span_attribute_update AFTER UPDATE OF data ON ef_telemetry_spans BEGIN DELETE FROM ef_telemetry_span_attributes WHERE trace_id=NEW.trace_id AND span_id=NEW.span_id; INSERT INTO ef_telemetry_span_attributes SELECT NEW.trace_id,NEW.span_id,json_extract(NEW.data,'$.name'),key,CAST(value AS REAL) FROM json_each(NEW.data,'$.attributes'); END;
        CREATE INDEX IF NOT EXISTS ef_telemetry_spans_parent ON ef_telemetry_spans(trace_id,json_extract(data,'$.parentSpanId'),span_id);
        CREATE INDEX IF NOT EXISTS ef_telemetry_agent_roots_time ON ef_telemetry_spans(json_extract(data,'$.startedAt') DESC,trace_id,span_id) WHERE json_extract(data,'$.name') IN ('tron.session.run','tron.dude.execute');
        CREATE INDEX IF NOT EXISTS ef_telemetry_agent_roots_workspace ON ef_telemetry_spans(json_extract(data,'$.attributes.\"tron.workspace.code\"'),json_extract(data,'$.startedAt') DESC,trace_id,span_id) WHERE json_extract(data,'$.name') IN ('tron.session.run','tron.dude.execute');
        CREATE INDEX IF NOT EXISTS ef_telemetry_agent_roots_session ON ef_telemetry_spans(json_extract(data,'$.attributes.\"tron.session.code\"'),json_extract(data,'$.startedAt') DESC,trace_id,span_id) WHERE json_extract(data,'$.name') IN ('tron.session.run','tron.dude.execute');
        CREATE INDEX IF NOT EXISTS ef_telemetry_page_roots_time ON ef_telemetry_traces(started_at DESC,trace_id) WHERE json_extract(attributes,'$.\"tron.trace.page\"')='true';
        CREATE TABLE IF NOT EXISTS ef_telemetry_span_rollups(trace_id TEXT NOT NULL REFERENCES ef_telemetry_traces(trace_id) ON DELETE CASCADE,span_id TEXT NOT NULL,parent_id TEXT,own_span_count INTEGER NOT NULL DEFAULT 0,own_error_count INTEGER NOT NULL DEFAULT 0,own_model_count INTEGER NOT NULL DEFAULT 0,own_tool_count INTEGER NOT NULL DEFAULT 0,own_input_tokens INTEGER NOT NULL DEFAULT 0,own_cached_input_tokens INTEGER NOT NULL DEFAULT 0,own_output_tokens INTEGER NOT NULL DEFAULT 0,span_count INTEGER NOT NULL DEFAULT 0,error_count INTEGER NOT NULL DEFAULT 0,model_count INTEGER NOT NULL DEFAULT 0,tool_count INTEGER NOT NULL DEFAULT 0,input_tokens INTEGER NOT NULL DEFAULT 0,cached_input_tokens INTEGER NOT NULL DEFAULT 0,output_tokens INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(trace_id,span_id));
        CREATE TABLE IF NOT EXISTS ef_telemetry_trace_rollups(trace_id TEXT PRIMARY KEY REFERENCES ef_telemetry_traces(trace_id) ON DELETE CASCADE,span_count INTEGER NOT NULL DEFAULT 0,error_count INTEGER NOT NULL DEFAULT 0,model_count INTEGER NOT NULL DEFAULT 0,tool_count INTEGER NOT NULL DEFAULT 0,input_tokens INTEGER NOT NULL DEFAULT 0,cached_input_tokens INTEGER NOT NULL DEFAULT 0,output_tokens INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE IF NOT EXISTS ef_telemetry_metric_points(point_id TEXT PRIMARY KEY,name TEXT NOT NULL,timestamp INTEGER NOT NULL,data TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS ef_telemetry_spans_order ON ef_telemetry_spans(trace_id,json_extract(data,'$.startedAt'),span_id);
        CREATE INDEX IF NOT EXISTS ef_telemetry_metric_time ON ef_telemetry_metric_points(timestamp DESC,point_id DESC);
        CREATE INDEX IF NOT EXISTS ef_telemetry_metric_name ON ef_telemetry_metric_points(name,timestamp DESC,point_id DESC);
        CREATE TABLE IF NOT EXISTS ef_telemetry_metric_attributes(point_id TEXT NOT NULL REFERENCES ef_telemetry_metric_points(point_id) ON DELETE CASCADE,attribute TEXT NOT NULL,value TEXT NOT NULL,name TEXT NOT NULL,timestamp INTEGER NOT NULL,PRIMARY KEY(attribute,value,point_id));
        CREATE INDEX IF NOT EXISTS ef_telemetry_metric_attribute_page ON ef_telemetry_metric_attributes(attribute,value,name,timestamp DESC,point_id DESC);
        CREATE INDEX IF NOT EXISTS ef_telemetry_metric_attribute_time ON ef_telemetry_metric_attributes(attribute,value,timestamp DESC,point_id DESC);
        CREATE INDEX IF NOT EXISTS ef_telemetry_metric_attribute_point ON ef_telemetry_metric_attributes(point_id);
        CREATE TRIGGER IF NOT EXISTS ef_telemetry_metric_attribute_insert AFTER INSERT ON ef_telemetry_metric_points BEGIN
          INSERT OR IGNORE INTO ef_telemetry_metric_attributes SELECT NEW.point_id,key,value,NEW.name,NEW.timestamp FROM json_each(NEW.data,'$.resource') WHERE type='text';
          INSERT OR IGNORE INTO ef_telemetry_metric_attributes SELECT NEW.point_id,json_extract(value,'$.key'),json_extract(value,'$.value.stringValue'),NEW.name,NEW.timestamp FROM json_each(NEW.data,'$.data.attributes') WHERE json_type(value,'$.value.stringValue')='text';
        END;
        UPDATE ef_telemetry_traces SET abandoned=1,ended_at=COALESCE((SELECT MAX(ended_at) FROM ef_telemetry_spans s WHERE s.trace_id=ef_telemetry_traces.trace_id),started_at) WHERE ended_at IS NULL;")?;
        if !had_span_attributes {c.execute_batch("INSERT OR IGNORE INTO ef_telemetry_span_attributes SELECT s.trace_id,s.span_id,json_extract(s.data,'$.name'),a.key,CAST(a.value AS REAL) FROM ef_telemetry_spans s,json_each(s.data,'$.attributes') a;")?;}
        if !had_trace_attributes {c.execute_batch("INSERT OR IGNORE INTO ef_telemetry_trace_attributes SELECT t.trace_id,a.key,a.value FROM ef_telemetry_traces t,json_each(t.attributes) a WHERE a.type='text';")?;}
        if !had_attributes {
            c.execute_batch("INSERT OR IGNORE INTO ef_telemetry_metric_attributes SELECT p.point_id,a.key,a.value,p.name,p.timestamp FROM ef_telemetry_metric_points p,json_each(p.data,'$.resource') a WHERE a.type='text';
            INSERT OR IGNORE INTO ef_telemetry_metric_attributes SELECT p.point_id,json_extract(a.value,'$.key'),json_extract(a.value,'$.value.stringValue'),p.name,p.timestamp FROM ef_telemetry_metric_points p,json_each(p.data,'$.data.attributes') a WHERE json_type(a.value,'$.value.stringValue')='text';")?;
        }
        Ok(())}).await.map_err(error)?;
        database.call(rollups::backfill).await.map_err(error)?;
        let store = Self {
            database,
            retention,
        };
        store.cleanup().await?;
        Ok(store)
    }
    pub(super) fn clean(c: &Connection, retention: &TelemetryRetention) -> rusqlite::Result<()> {
        c.execute("DELETE FROM ef_telemetry_traces WHERE ended_at IS NOT NULL AND COALESCE(ended_at,started_at)<?1",[now().saturating_sub(retention.trace_max_age.as_millis() as i64)])?;
        if let Some(limit) = retention.max_completed_traces {
            c.execute("DELETE FROM ef_telemetry_traces WHERE trace_id IN(SELECT trace_id FROM ef_telemetry_traces WHERE ended_at IS NOT NULL ORDER BY COALESCE(ended_at,started_at) DESC,trace_id DESC LIMIT -1 OFFSET ?1)",[limit])?;
        }
        c.execute(
            "DELETE FROM ef_telemetry_metric_points WHERE timestamp<?1",
            [now().saturating_sub(retention.metric_max_age.as_millis() as i64)],
        )?;
        Ok(())
    }
}
#[async_trait]
impl TelemetryStore for SqliteTelemetryStore {
    async fn register_trace(&self, t: TelemetryTrace) -> Result<(), TelemetryError> {
        self.database
            .call(move |c| -> rusqlite::Result<()> {
                c.execute(
                    "INSERT INTO ef_telemetry_traces VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(trace_id) DO UPDATE SET started_at=MIN(started_at,excluded.started_at),ended_at=excluded.ended_at,status_code=excluded.status_code,attributes=json_patch(attributes,excluded.attributes)",
                    params![
                        t.trace_id,
                        t.started_at,
                        t.ended_at,
                        t.status_code,
                        serde_json::to_string(&t.attributes)
                            .map_err(|_| rusqlite::Error::InvalidQuery)?,
                        t.abandoned
                    ],
                )?;
                Ok(())
            })
            .await
            .map_err(error)
    }
    async fn write_spans(&self, spans: Vec<TelemetrySpan>) -> Result<(), TelemetryError> {
        let policy = self.retention.clone();
        self.database.call(move|c|->rusqlite::Result<()> {
            let tx=c.transaction()?;
            for s in spans {
                let root=s.parent_span_id.is_none();
                tx.execute("INSERT INTO ef_telemetry_traces VALUES(?1,?2,?3,?4,?5,0) ON CONFLICT(trace_id) DO UPDATE SET started_at=MIN(started_at,excluded.started_at),ended_at=CASE WHEN ?6 THEN CASE WHEN ended_at IS NULL THEN excluded.ended_at ELSE MAX(ended_at,excluded.ended_at) END ELSE ended_at END,status_code=CASE WHEN ?6 THEN MAX(status_code,excluded.status_code) ELSE status_code END,attributes=CASE WHEN ?6 THEN json_patch(attributes,excluded.attributes) ELSE attributes END,abandoned=CASE WHEN ?6 THEN 0 ELSE abandoned END",params![s.trace_id,s.started_at,root.then_some(s.ended_at),s.status_code,serde_json::to_string(&s.attributes).map_err(|_|rusqlite::Error::InvalidQuery)?,root])?;
                rollups::write(&tx,&s)?;
                let json=serde_json::to_string(&s).map_err(|_|rusqlite::Error::InvalidQuery)?;
                tx.execute("INSERT OR REPLACE INTO ef_telemetry_spans VALUES(?1,?2,?3,?4)",params![s.trace_id,s.span_id,s.ended_at,json])?;
            }
            Self::clean(&tx,&policy)?; tx.commit()
        }).await.map_err(error)
    }
    async fn write_metrics(&self, points: Vec<TelemetryMetricPoint>) -> Result<(), TelemetryError> {
        let policy = self.retention.clone();
        self.database
            .call(move |c| -> rusqlite::Result<()> {
                let tx = c.transaction()?;
                for p in points {
                    let data =
                        serde_json::to_string(&p).map_err(|_| rusqlite::Error::InvalidQuery)?;
                    tx.execute(
                        "INSERT OR IGNORE INTO ef_telemetry_metric_points VALUES(?1,?2,?3,?4)",
                        params![p.point_id, p.name, p.timestamp, data],
                    )?;
                }
                Self::clean(&tx, &policy)?;
                tx.commit()
            })
            .await
            .map_err(error)
    }
    async fn query_traces(
        &self,
        f: TelemetryTraceFilter,
        offset: usize,
        limit: usize,
    ) -> Result<TelemetryPage<TelemetryTrace>, TelemetryError> {
        query::traces(self, f, offset, limit).await
    }
    async fn query_spans(
        &self,
        ids: Vec<String>,
        offset: usize,
        limit: usize,
    ) -> Result<TelemetryPage<TelemetrySpan>, TelemetryError> {
        query::span_page(self, ids, offset, limit).await
    }
    async fn aggregate_traces(
        &self,
        filter: TelemetryTraceFilter,
        classifications: std::collections::BTreeMap<String, TelemetryTracePredicate>,
        buckets: Option<TelemetryBucketQuery>,
    ) -> Result<TelemetryTraceStatistics, TelemetryError> {
        aggregate::query(self, filter, classifications, buckets).await
    }
    async fn spans(&self, id: String) -> Result<Vec<TelemetrySpan>, TelemetryError> {
        query::spans(self, id).await
    }
    async fn sum_span_attributes(
        &self,
        f: TelemetryTraceFilter,
        name: String,
        attrs: Vec<String>,
    ) -> Result<std::collections::BTreeMap<String, u64>, TelemetryError> {
        query::sums(self, f, name, attrs).await
    }
    async fn metric_points(
        &self,
        filter: TelemetryMetricFilter,
        offset: usize,
        limit: usize,
    ) -> Result<TelemetryPage<TelemetryMetricPoint>, TelemetryError> {
        query::points(self, filter, offset, limit).await
    }
    async fn count_metric_points_by_attribute(
        &self,
        attribute: String,
        value: String,
    ) -> Result<usize, TelemetryError> {
        query::count_points_by_attribute(self, attribute, value).await
    }
    async fn latest_metric(
        &self,
        name: String,
        resource: std::collections::BTreeMap<String, String>,
    ) -> Result<Option<TelemetryMetricPoint>, TelemetryError> {
        query::latest(self, name, resource).await
    }
    async fn cleanup(&self) -> Result<(), TelemetryError> {
        let policy = self.retention.clone();
        self.database
            .call(move |c| -> rusqlite::Result<()> {
                let tx = c.transaction()?;
                Self::clean(&tx, &policy)?;
                tx.commit()
            })
            .await
            .map_err(error)
    }
}
#[derive(Clone)]
pub struct TelemetryStoragePlugin {
    path: PathBuf,
    retention: TelemetryRetention,
}
impl TelemetryStoragePlugin {
    pub fn new(path: impl Into<PathBuf>, retention: TelemetryRetention) -> Self {
        Self {
            path: path.into(),
            retention,
        }
    }
}
#[async_trait]
impl Plugin for TelemetryStoragePlugin {
    fn id(&self) -> &str {
        "storage.telemetry"
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let store = SqliteTelemetryStore::open(self.path.clone(), self.retention.clone())
            .await
            .map_err(|e| PluginError::Message(e.to_string()))?;
        let database = store.database.clone();
        context
            .on_shutdown(move || async move { database.close().await.map_err(|e| e.to_string()) });
        context.provide(TelemetryStoreService(Arc::new(store)))
    }
}
