//! JSON bridge transport. The host owns request/response projections; this is
//! not an implementation of a vendor's app-server or ACP wire protocol.
//! Dropping an invocation stops local waiting. With a known remote run ID, it
//! also sends a best-effort cancellation request; an HTTP success is not proof
//! that the remote execution has stopped.
use crate::TransportError;
use serde_json::Value;

// Drop cleanup must not retain a task or socket indefinitely when the bridge
// accepts a cancellation connection but never responds.
const CANCEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Clone)]
pub struct JsonAgentBridge {
    endpoint: reqwest::Url,
    client: reqwest::Client,
}
impl JsonAgentBridge {
    pub fn new(endpoint: &str, client: reqwest::Client) -> Result<Self, TransportError> {
        let mut endpoint = reqwest::Url::parse(endpoint.trim())
            .map_err(|error| TransportError::Failed(format!("invalid bridge endpoint: {error}")))?;
        if !matches!(endpoint.scheme(), "http" | "https") || endpoint.host_str().is_none() {
            return Err(TransportError::Failed(
                "bridge endpoint must use HTTP or HTTPS".into(),
            ));
        }
        endpoint.set_query(None);
        endpoint.set_fragment(None);
        Ok(Self { endpoint, client })
    }
    fn url(&self, segments: &[&str]) -> Result<reqwest::Url, TransportError> {
        let mut url = self.endpoint.clone();
        url.path_segments_mut()
            .map_err(|_| TransportError::Failed("invalid bridge base".into()))?
            .pop_if_empty()
            .extend(segments.iter().copied());
        Ok(url)
    }
    pub async fn invoke(&self, resume: bool, body: &Value) -> Result<Value, TransportError> {
        let url = self.url(&["runs", if resume { "resume" } else { "start" }])?;
        let response = self
            .client
            .post(url)
            .json(body)
            .send()
            .await
            .map_err(|error| TransportError::Failed(format!("bridge request failed: {error}")))?;
        let status = response.status();
        let value: Value = response.json().await.map_err(|error| {
            TransportError::Failed(format!("bridge response parse failed: {error}"))
        })?;
        if !status.is_success() {
            return Err(TransportError::Failed(format!(
                "bridge returned status {}",
                status.as_u16()
            )));
        }
        Ok(value.get("data").cloned().unwrap_or(value))
    }
    /// Invoke with a run ID already known before the request starts. When the
    /// waiting future is dropped, try to cancel that remote run in the
    /// background. Newly started runs have no such ID until the response and
    /// therefore cannot be remotely cancelled by this protocol.
    pub async fn invoke_with_known_run(
        &self,
        resume: bool,
        body: &Value,
        known_run_id: Option<&str>,
        cancel_body: &Value,
    ) -> Result<Value, TransportError> {
        let mut guard =
            known_run_id
                .filter(|id| !id.trim().is_empty())
                .map(|id| CancelKnownRunOnDrop {
                    bridge: self.clone(),
                    run_id: id.to_owned(),
                    body: cancel_body.clone(),
                    armed: true,
                });
        let result = self.invoke(resume, body).await;
        if let Some(guard) = guard.as_mut() {
            guard.armed = false;
        }
        result
    }
    /// A successful response means only that the bridge accepted the request.
    /// The caller must still treat remote stop as unconfirmed.
    pub async fn cancel(&self, run_id: &str, body: &Value) -> Result<(), TransportError> {
        let response = self
            .client
            .post(self.url(&["runs", run_id, "cancel"])?)
            .timeout(CANCEL_TIMEOUT)
            .json(body)
            .send()
            .await
            .map_err(|error| TransportError::Failed(format!("bridge cancel failed: {error}")))?;
        if !response.status().is_success() {
            return Err(TransportError::Failed(format!(
                "bridge cancel returned status {}",
                response.status().as_u16()
            )));
        }
        Ok(())
    }
}

struct CancelKnownRunOnDrop {
    bridge: JsonAgentBridge,
    run_id: String,
    body: Value,
    armed: bool,
}

impl Drop for CancelKnownRunOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let bridge = self.bridge.clone();
            let run_id = self.run_id.clone();
            let body = self.body.clone();
            runtime.spawn(async move {
                let _ = bridge.cancel(&run_id, &body).await;
            });
        }
    }
}
