//! OTLP/HTTP protobuf receiver; storage is injected through a host-neutral port.
//! The host owns tenant identity and access policy.
mod decode;
use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use eliteforge_agent_core::TelemetryStore;
use opentelemetry_proto::tonic::collector::{
    metrics::v1::{ExportMetricsServiceRequest, ExportMetricsServiceResponse},
    trace::v1::{ExportTraceServiceRequest, ExportTraceServiceResponse},
};
use prost::Message;
use std::sync::Arc;
#[derive(Clone)]
struct CollectorState {
    pub store: Arc<dyn TelemetryStore>,
    pub authorization: String,
    pub permits: Arc<tokio::sync::Semaphore>,
}
fn routes(state: CollectorState) -> Router {
    Router::new()
        .route("/v1/traces", post(traces))
        .route("/v1/metrics", post(metrics))
        .layer(DefaultBodyLimit::max(8 * 1024 * 1024))
        .with_state(state)
}
fn authorize(headers: &HeaderMap, state: &CollectorState) -> Result<(), StatusCode> {
    if headers
        .get(header::AUTHORIZATION)
        .and_then(|x| x.to_str().ok())
        != Some(&state.authorization)
    {
        return Err(StatusCode::UNAUTHORIZED);
    }
    if headers
        .get(header::CONTENT_ENCODING)
        .is_some_and(|h| h != "identity" && h != "gzip")
    {
        return Err(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    if headers
        .get(header::CONTENT_TYPE)
        .and_then(|x| x.to_str().ok())
        .and_then(|x| x.split(';').next())
        != Some("application/x-protobuf")
    {
        return Err(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    Ok(())
}
fn protobuf(message: impl Message) -> Response {
    (
        [(header::CONTENT_TYPE, "application/x-protobuf")],
        message.encode_to_vec(),
    )
        .into_response()
}
async fn traces(State(state): State<CollectorState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(code) = authorize(&headers, &state) {
        return code.into_response();
    }
    let Ok(_permit) = state.permits.clone().try_acquire_owned() else {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    };
    let body = match decode_body(body, &headers) {
        Ok(body) => body,
        Err(code) => return code.into_response(),
    };
    let records = ExportTraceServiceRequest::decode(body.as_slice())
        .ok()
        .and_then(|request| decode::spans(request).ok());
    let Some(records) = records else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let store = state.store;
    // Acknowledge only after the database transaction commits; failed writes are retryable.
    match store.write_spans(records).await {
        Ok(()) => protobuf(ExportTraceServiceResponse::default()),
        _ => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}
async fn metrics(State(state): State<CollectorState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(code) = authorize(&headers, &state) {
        return code.into_response();
    }
    let Ok(_permit) = state.permits.clone().try_acquire_owned() else {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    };
    let body = match decode_body(body, &headers) {
        Ok(body) => body,
        Err(code) => return code.into_response(),
    };
    let points = ExportMetricsServiceRequest::decode(body.as_slice())
        .ok()
        .and_then(|request| decode::metrics(request).ok());
    let Some(points) = points else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let store = state.store;
    match store.write_metrics(points).await {
        Ok(()) => protobuf(ExportMetricsServiceResponse::default()),
        _ => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

fn decode_body(body: Bytes, headers: &HeaderMap) -> Result<Vec<u8>, StatusCode> {
    use std::io::Read;
    if headers
        .get(header::CONTENT_ENCODING)
        .is_some_and(|h| h == "gzip")
    {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(body.as_ref())
            .take(8 * 1024 * 1024 + 1)
            .read_to_end(&mut out)
            .map_err(|_| StatusCode::BAD_REQUEST)?;
        if out.len() > 8 * 1024 * 1024 {
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }
        Ok(out)
    } else {
        Ok(body.to_vec())
    }
}

/// Host-provided listening address and credential. No tenant/profile concepts are embedded.
#[derive(Clone)]
pub struct CollectorConfig {
    pub bind: std::net::SocketAddr,
    pub authorization: String,
    pub max_concurrent_requests: usize,
    pub cleanup_interval: std::time::Duration,
}
#[derive(Clone)]
pub struct CollectorService {
    pub endpoint: String,
    pub authorization: String,
}
pub struct CollectorPlugin {
    pub config: CollectorConfig,
    pub storage_plugin_id: String,
}
#[async_trait::async_trait]
impl eliteforge_agent_core::Plugin for CollectorPlugin {
    fn id(&self) -> &str {
        "otel.collector"
    }
    fn dependencies(&self) -> Vec<String> {
        vec![self.storage_plugin_id.clone()]
    }
    async fn install(
        &self,
        context: &mut eliteforge_agent_core::PluginContext<'_>,
    ) -> Result<(), eliteforge_agent_core::PluginError> {
        use eliteforge_agent_core::{PluginError, TelemetryStoreService};
        if self.config.authorization.trim().is_empty()
            || self.config.max_concurrent_requests == 0
            || self.config.cleanup_interval.is_zero()
        {
            return Err(PluginError::Message(
                "invalid collector configuration".into(),
            ));
        }
        let store = context
            .services()
            .get::<TelemetryStoreService>()
            .ok_or_else(|| PluginError::Message("telemetry store required".into()))?
            .0
            .clone();
        let listener = tokio::net::TcpListener::bind(self.config.bind)
            .await
            .map_err(|e| PluginError::Message(e.to_string()))?;
        let endpoint = format!(
            "http://{}",
            listener
                .local_addr()
                .map_err(|e| PluginError::Message(e.to_string()))?
        );
        let stop = tokio_util::sync::CancellationToken::new();
        let fallback = stop.clone();
        context.on_cleanup(move || fallback.cancel());
        let server_stop = stop.clone();
        let router = routes(CollectorState {
            store: store.clone(),
            authorization: self.config.authorization.clone(),
            permits: Arc::new(tokio::sync::Semaphore::new(
                self.config.max_concurrent_requests,
            )),
        });
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(server_stop.cancelled_owned())
                .await
                .map_err(|e| e.to_string())
        });
        let clean_stop = stop.clone();
        let period = self.config.cleanup_interval;
        let cleaner = tokio::spawn(async move {
            let mut timer = tokio::time::interval(period);
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {_=clean_stop.cancelled()=>break,_=timer.tick()=>{if let Err(e)=store.cleanup().await {eprintln!("telemetry cleanup failed: {e}");}}}
            }
        });
        context.on_shutdown(move || async move {
            stop.cancel();
            let result = server.await.map_err(|e| e.to_string())?;
            cleaner.await.map_err(|e| e.to_string())?;
            result
        });
        context.provide(CollectorService {
            endpoint,
            authorization: self.config.authorization.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eliteforge_agent_core::TelemetryRetention;
    use eliteforge_agent_official_plugin_storage_local::{SqliteDatabase, SqliteTelemetryStore};
    use std::time::Duration;

    #[tokio::test]
    async fn closed_storage_returns_retryable_failure_for_both_signals() {
        let temp = tempfile::tempdir().unwrap();
        let db = SqliteDatabase::open(temp.path().join("closed.db"))
            .await
            .unwrap();
        let store = SqliteTelemetryStore::with_database(
            db.clone(),
            TelemetryRetention {
                trace_max_age: Duration::from_secs(86400),
                metric_max_age: Duration::from_secs(86400),
                max_completed_traces: None,
            },
        )
        .await
        .unwrap();
        db.close().await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = routes(CollectorState {
            store: Arc::new(store),
            authorization: "Bearer test".into(),
            permits: Arc::new(tokio::sync::Semaphore::new(1)),
        });
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        // An empty protobuf export is valid; its storage transaction still must succeed.
        for signal in ["traces", "metrics"] {
            let response = client
                .post(format!("http://{address}/v1/{signal}"))
                .header("authorization", "Bearer test")
                .header("content-type", "application/x-protobuf")
                .body(Vec::new())
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        }
        server.abort();
        let _ = server.await;
    }
}
