//! JSON bridge transport. The host owns request/response projections; this is
//! not an implementation of a vendor's app-server or ACP wire protocol.
use crate::TransportError;
use serde_json::Value;

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
    pub async fn cancel(&self, run_id: &str, body: &Value) -> Result<(), TransportError> {
        let response = self
            .client
            .post(self.url(&["runs", run_id, "cancel"])?)
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
