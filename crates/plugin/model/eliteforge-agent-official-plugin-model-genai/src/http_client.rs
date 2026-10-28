use eliteforge_agent_core::ModelError;
use std::time::Duration;

/// Host-owned transport policy shared by native and compatible API adapters.
/// Neither credentials nor model/provider catalogue data belong here.
#[derive(Clone, Default)]
pub struct ModelHttpClientOptions {
    pub connect_timeout: Option<Duration>,
    pub timeout: Option<Duration>,
    pub read_timeout: Option<Duration>,
    pub pool_max_idle_per_host: Option<usize>,
    pub pool_idle_timeout: Option<Duration>,
    pub tcp_keepalive: Option<Duration>,
    pub proxy_url: Option<String>,
    pub accept_invalid_certificates: bool,
}

#[derive(Clone)]
pub struct GenaiHttpClient(pub(crate) genai::Client);

impl GenaiHttpClient {
    pub fn from_options(options: &ModelHttpClientOptions) -> Result<Self, ModelError> {
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .danger_accept_invalid_certs(options.accept_invalid_certificates);
        if let Some(value) = options.connect_timeout {
            builder = builder.connect_timeout(value);
        }
        if let Some(value) = options.timeout {
            builder = builder.timeout(value);
        }
        if let Some(value) = options.read_timeout {
            builder = builder.read_timeout(value);
        }
        if let Some(value) = options.pool_max_idle_per_host {
            builder = builder.pool_max_idle_per_host(value);
        }
        if let Some(value) = options.pool_idle_timeout {
            builder = builder.pool_idle_timeout(value);
        }
        if let Some(value) = options.tcp_keepalive {
            builder = builder.tcp_keepalive(value);
        }
        if let Some(value) = &options.proxy_url {
            builder = builder.proxy(
                reqwest::Proxy::all(value)
                    .map_err(|_| ModelError::Fatal("invalid model proxy configuration".into()))?,
            );
        }
        let client = builder
            .build()
            .map_err(|_| ModelError::Fatal("model HTTP client initialization failed".into()))?;
        genai::Client::builder()
            .with_reqwest(client)
            .build()
            .map(Self)
            .map_err(|_| ModelError::Fatal("model protocol client initialization failed".into()))
    }
}
