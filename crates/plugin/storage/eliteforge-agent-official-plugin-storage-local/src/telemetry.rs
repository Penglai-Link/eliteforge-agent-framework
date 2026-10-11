//! Dedicated telemetry storage plugin using the shared SQLite runtime.
mod query;
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
        database.call(|c|->rusqlite::Result<()> {c.execute_batch("CREATE TABLE IF NOT EXISTS ef_telemetry_traces (trace_id TEXT PRIMARY KEY,started_at INTEGER NOT NULL,ended_at INTEGER,status_code INTEGER NOT NULL,attributes TEXT NOT NULL,abandoned INTEGER NOT NULL DEFAULT 0);
        CREATE INDEX IF NOT EXISTS ef_telemetry_traces_time ON ef_telemetry_traces(started_at DESC);
        CREATE INDEX IF NOT EXISTS ef_telemetry_traces_retention ON ef_telemetry_traces(COALESCE(ended_at,started_at) DESC,trace_id DESC) WHERE ended_at IS NOT NULL;
        CREATE TABLE IF NOT EXISTS ef_telemetry_spans(trace_id TEXT NOT NULL REFERENCES ef_telemetry_traces(trace_id) ON DELETE CASCADE,span_id TEXT NOT NULL,ended_at INTEGER NOT NULL,data TEXT NOT NULL,PRIMARY KEY(trace_id,span_id));
        CREATE TABLE IF NOT EXISTS ef_telemetry_metric_points(point_id TEXT PRIMARY KEY,name TEXT NOT NULL,timestamp INTEGER NOT NULL,data TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS ef_telemetry_metric_time ON ef_telemetry_metric_points(timestamp DESC);
        CREATE INDEX IF NOT EXISTS ef_telemetry_metric_name ON ef_telemetry_metric_points(name,timestamp DESC);
        UPDATE ef_telemetry_traces SET abandoned=1,ended_at=COALESCE((SELECT MAX(ended_at) FROM ef_telemetry_spans s WHERE s.trace_id=ef_telemetry_traces.trace_id),started_at) WHERE ended_at IS NULL;")}).await.map_err(error)?;
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
        name: Option<String>,
        offset: usize,
        limit: usize,
    ) -> Result<TelemetryPage<TelemetryMetricPoint>, TelemetryError> {
        query::points(self, name, offset, limit).await
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
