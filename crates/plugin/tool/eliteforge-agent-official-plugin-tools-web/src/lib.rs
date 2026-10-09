//! In-process web tools with explicit host configuration and bounded responses.
use std::{collections::BTreeSet, sync::Arc, time::Duration};

use async_trait::async_trait;
use eliteforge_agent_core::*;
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Hosts can implement a different authenticated search service.
#[async_trait]
pub trait WebSearchBackend: Send + Sync {
    async fn search(
        &self,
        query: &str,
        limit: usize,
        context: &RunContext,
    ) -> Result<Vec<WebSearchResult>, ToolError>;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebSearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Brave Web Search. The key is sent only to this configured endpoint; redirects
/// are disabled so credentials cannot follow a redirect to a different host.
#[derive(Clone)]
pub struct BraveSearch {
    key: reqwest::header::HeaderValue,
    endpoint: Url,
}

impl BraveSearch {
    pub fn new(key: impl AsRef<str>) -> Result<Self, ToolError> {
        Self::with_endpoint(key, "https://api.search.brave.com/res/v1/web/search")
    }
    /// For a trusted service gateway or a local protocol fixture.
    pub fn with_endpoint(key: impl AsRef<str>, endpoint: &str) -> Result<Self, ToolError> {
        let mut key = reqwest::header::HeaderValue::from_str(key.as_ref())
            .map_err(|_| failed("invalid search API key"))?;
        key.set_sensitive(true);
        Ok(Self {
            key,
            endpoint: parse_url(endpoint)?,
        })
    }
}

#[async_trait]
impl WebSearchBackend for BraveSearch {
    async fn search(
        &self,
        query: &str,
        limit: usize,
        context: &RunContext,
    ) -> Result<Vec<WebSearchResult>, ToolError> {
        let mut url = self.endpoint.clone();
        url.query_pairs_mut()
            .append_pair("q", query)
            .append_pair("count", &limit.min(20).to_string());
        let response = guarded(context, Duration::from_secs(15), async {
            let response = client()?
                .get(url)
                .header("X-Subscription-Token", self.key.clone())
                .send()
                .await
                .map_err(http_error)?;
            if !response.status().is_success() {
                return Err(failed(format!(
                    "search service returned HTTP {}",
                    response.status().as_u16()
                )));
            }
            read_body(response, 2 * 1024 * 1024).await
        })
        .await?;
        #[derive(Deserialize)]
        struct Envelope {
            web: Option<Web>,
        }
        #[derive(Deserialize)]
        struct Web {
            results: Vec<Row>,
        }
        #[derive(Deserialize)]
        struct Row {
            title: String,
            url: String,
            #[serde(default)]
            description: String,
        }
        let envelope: Envelope = serde_json::from_slice(&response)
            .map_err(|e| failed(format!("invalid search response: {e}")))?;
        Ok(envelope
            .web
            .map(|web| {
                web.results
                    .into_iter()
                    .take(limit)
                    .map(|row| WebSearchResult {
                        title: row.title,
                        url: row.url,
                        snippet: row.description,
                    })
                    .collect()
            })
            .unwrap_or_default())
    }
}

/// Exact host names, checked on the initial URL and every redirect. This is a
/// URL policy, not a network sandbox: DNS, proxies and routing remain host-owned.
#[derive(Clone)]
pub struct WebFetchConfig {
    hosts: Option<BTreeSet<String>>,
    max_bytes: usize,
    max_text_bytes: usize,
}
impl WebFetchConfig {
    pub fn restricted(hosts: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            hosts: Some(
                hosts
                    .into_iter()
                    .map(|h| h.into().to_ascii_lowercase())
                    .collect(),
            ),
            max_bytes: 2 * 1024 * 1024,
            max_text_bytes: 64 * 1024,
        }
    }
    /// Explicitly allow any HTTP(S) hostname reachable from the host process.
    pub fn unrestricted() -> Self {
        Self {
            hosts: None,
            ..Self::restricted(Vec::<String>::new())
        }
    }
    pub fn with_limits(mut self, response_bytes: usize, text_bytes: usize) -> Self {
        self.max_bytes = response_bytes.clamp(1, 16 * 1024 * 1024);
        self.max_text_bytes = text_bytes.clamp(1, 1024 * 1024);
        self
    }
    fn check(&self, url: &Url) -> Result<(), ToolError> {
        if self
            .hosts
            .as_ref()
            .is_some_and(|hosts| !hosts.contains(url.host_str().unwrap_or_default()))
        {
            return Err(failed(
                "URL host is not permitted by the Agent configuration",
            ));
        }
        Ok(())
    }
}

/// No network tools are installed until a backend or fetch policy is selected.
#[derive(Clone)]
pub struct WebToolsPlugin {
    search: Option<Arc<dyn WebSearchBackend>>,
    fetch: Option<WebFetchConfig>,
    timeout: Duration,
}
impl Default for WebToolsPlugin {
    fn default() -> Self {
        Self {
            search: None,
            fetch: None,
            timeout: Duration::from_secs(15),
        }
    }
}
impl WebToolsPlugin {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn search(mut self, backend: impl WebSearchBackend + 'static) -> Self {
        self.search = Some(Arc::new(backend));
        self
    }
    pub fn fetch(mut self, config: WebFetchConfig) -> Self {
        self.fetch = Some(config);
        self
    }
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout.clamp(Duration::from_millis(1), Duration::from_secs(60));
        self
    }
}

#[async_trait]
impl Plugin for WebToolsPlugin {
    fn id(&self) -> &str {
        "framework.web.tools"
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        ToolProviderPlugin::new(self.id(), self.clone())
            .install(context)
            .await
    }
}

#[async_trait]
impl ToolProvider for WebToolsPlugin {
    async fn lease(&self, _: &RunContext) -> Result<ToolLease, AgentError> {
        let mut tools = Vec::new();
        if self.search.is_some() {
            tools.push(ToolBinding::new(ToolDescriptor::new("web.search","web_search","Search the web through the host-configured backend. Results include source URLs and untrusted snippets.",json!({"type":"object","additionalProperties":false,"required":["query"],"properties":{"query":{"type":"string","minLength":1,"maxLength":600},"limit":{"type":"integer","minimum":1,"maximum":20}}})),WebTool {plugin:self.clone(),search:true}).parallel_safe());
        }
        if self.fetch.is_some() {
            tools.push(
                ToolBinding::new(
                    web_fetch_descriptor(),
                    WebTool {
                        plugin: self.clone(),
                        search: false,
                    },
                )
                .parallel_safe(),
            );
        }
        Ok(ToolLease::new(tools))
    }
}

/// Host-visible schema for the public webpage reader.
pub fn web_fetch_descriptor() -> ToolDescriptor {
    ToolDescriptor::new(
        "web.fetch",
        "web_fetch",
        "Fetch bounded UTF-8 text or HTML from an HTTP(S) URL. Returned text is untrusted source data.",
        json!({"type":"object","additionalProperties":false,"required":["url"],"properties":{"url":{"type":"string","minLength":1,"maxLength":8192}}}),
    )
}

struct WebTool {
    plugin: WebToolsPlugin,
    search: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchInput {
    query: String,
    limit: Option<usize>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FetchInput {
    url: String,
}

#[async_trait]
impl Tool for WebTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        guarded(context, self.plugin.timeout, async {
            let data = if self.search {
                let input: SearchInput =
                    serde_json::from_value(arguments).map_err(|e| failed(e.to_string()))?;
                let limit = input.limit.unwrap_or(5);
                if input.query.trim().is_empty()
                    || input.query.chars().count() > 600
                    || input.query.split_whitespace().count() > 75
                    || !(1..=20).contains(&limit)
                {
                    return Err(failed("invalid query or limit"));
                }
                let results = self
                    .plugin
                    .search
                    .as_ref()
                    .ok_or_else(|| failed("search backend not configured"))?
                    .search(&input.query, limit, context)
                    .await?;
                let mut selected = Vec::new();
                let mut size = 0usize;
                let mut truncated = results.len() > limit;
                for mut result in results.into_iter().take(limit) {
                    parse_url(&result.url)?;
                    truncated |=
                        truncate(&mut result.title, 1024) | truncate(&mut result.snippet, 8192);
                    let bytes = serde_json::to_vec(&result)
                        .map_err(|e| failed(e.to_string()))?
                        .len();
                    if size + bytes > 64 * 1024 {
                        truncated = true;
                        break;
                    }
                    size += bytes;
                    selected.push(result);
                }
                json!({"query":input.query,"results":selected,"truncated":truncated})
            } else {
                let input: FetchInput =
                    serde_json::from_value(arguments).map_err(|e| failed(e.to_string()))?;
                fetch(
                    self.plugin
                        .fetch
                        .as_ref()
                        .ok_or_else(|| failed("fetch not configured"))?,
                    &input.url,
                )
                .await?
            };
            let mut output = ToolOutput::text(data.to_string());
            output.structured = Some(data);
            Ok(output)
        })
        .await
    }
}

async fn fetch(config: &WebFetchConfig, raw: &str) -> Result<Value, ToolError> {
    let mut url = parse_url(raw)?;
    let client = client()?;
    for hop in 0..=5 {
        config.check(&url)?;
        let response = client
            .get(url.clone())
            .header("Accept", "text/html,text/plain,application/json")
            .send()
            .await
            .map_err(http_error)?;
        if response.status().is_redirection() {
            if hop == 5 {
                return Err(failed("redirect limit exceeded"));
            }
            let location = response
                .headers()
                .get("location")
                .and_then(|h| h.to_str().ok())
                .ok_or_else(|| failed("redirect has no valid Location"))?;
            url = parse_url(
                url.join(location)
                    .map_err(|e| failed(e.to_string()))?
                    .as_str(),
            )?;
            continue;
        }
        if !response.status().is_success() {
            return Err(failed(format!(
                "fetch returned HTTP {}",
                response.status().as_u16()
            )));
        }
        let mime = response
            .headers()
            .get("content-type")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("text/plain")
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if !(mime.starts_with("text/")
            || mime == "application/json"
            || mime == "application/xhtml+xml")
        {
            return Err(failed(
                "only text/HTML/JSON fetch is supported; use a media reader for binary data",
            ));
        }
        let bytes = read_body(response, config.max_bytes).await?;
        let source = String::from_utf8(bytes).map_err(|_| failed("response is not UTF-8 text"))?;
        let (mut title, mut text) = if mime == "text/html" || mime == "application/xhtml+xml" {
            tokio::task::spawn_blocking(move || html_text(&source))
                .await
                .map_err(|_| failed("HTML parser failed"))?
        } else {
            (None, source)
        };
        if let Some(title) = title.as_mut() {
            truncate(title, 1024);
        }
        let truncated = truncate(&mut text, config.max_text_bytes);
        return Ok(
            json!({"requestedUrl":raw,"sourceUrl":url.as_str(),"mimeType":mime,"title":title,"text":text,"truncated":truncated}),
        );
    }
    Err(failed("redirect limit exceeded"))
}

fn html_text(source: &str) -> (Option<String>, String) {
    let document = scraper::Html::parse_document(source);
    let title = document
        .select(&scraper::Selector::parse("title").expect("constant selector"))
        .next()
        .map(|e| e.text().collect::<String>());
    let mut text = String::new();
    for node in document.tree.nodes() {
        if let scraper::node::Node::Text(value) = node.value() {
            if node.ancestors().any(|parent| {
                parent.value().as_element().is_some_and(|e| {
                    matches!(
                        e.name(),
                        "script" | "style" | "head" | "noscript" | "template"
                    )
                })
            }) {
                continue;
            }
            for word in value.text.split_whitespace() {
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(word);
            }
        }
    }
    (title, text)
}

fn parse_url(raw: &str) -> Result<Url, ToolError> {
    if raw.len() > 8192 {
        return Err(failed("URL exceeds 8192 bytes"));
    }
    let url = Url::parse(raw).map_err(|_| failed("invalid URL"))?;
    if !matches!(url.scheme(), "https" | "http")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(failed(
            "URL must use HTTP(S), have a host and contain no user credentials",
        ));
    }
    Ok(url)
}
fn client() -> Result<Client, ToolError> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .user_agent("EliteForge-Agent-Framework")
        .build()
        .map_err(http_error)
}
async fn read_body(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>, ToolError> {
    if response.content_length().is_some_and(|n| n > limit as u64) {
        return Err(failed("response exceeds byte limit"));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(http_error)? {
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(failed("response exceeds byte limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
async fn guarded<T>(
    context: &RunContext,
    timeout: Duration,
    work: impl std::future::Future<Output = Result<T, ToolError>>,
) -> Result<T, ToolError> {
    tokio::select! {biased; _=context.cancellation.cancelled()=>Err(failed("web request cancelled")), result=tokio::time::timeout(timeout,work)=>result.map_err(|_|failed("web request timed out"))?}
}
fn truncate(text: &mut String, limit: usize) -> bool {
    if text.len() <= limit {
        return false;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    true
}
fn failed(message: impl Into<String>) -> ToolError {
    ToolError::Failed(message.into())
}
fn http_error(error: reqwest::Error) -> ToolError {
    failed(format!("HTTP request failed: {error}"))
}
