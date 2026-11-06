//! GitHub.com implementation of the portable connector capabilities.
//!
//! Remote mutations are sent once. A transport failure or a server error after
//! sending a mutation is reported as indeterminate rather than retried.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use eliteforge_agent_connector::{
    Comment, ConnectedProvider, Connection, ConnectorError, ConnectorProvider, Credential, Item,
    Page, PageRequest, ProviderDescriptor, Repository, RepositoryReader, ReviewReader,
    ReviewWriter, TaskReader, TaskWriter, TreeEntry,
};
use reqwest::{Client, Method, Response, Url, header};
use serde_json::{Value, json};

const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

pub struct GitHubProvider;

#[async_trait]
impl ConnectorProvider for GitHubProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: "github".into(),
            display_name: "GitHub".into(),
        }
    }

    async fn connect(
        &self,
        connection: &Connection,
        credential: Credential,
    ) -> Result<Arc<dyn ConnectedProvider>, ConnectorError> {
        // The initial implementation intentionally supports GitHub.com only.
        let base = Url::parse(&connection.endpoint).map_err(|_| ConnectorError::Configuration)?;
        if base.as_str().trim_end_matches('/') != "https://api.github.com" {
            return Err(ConnectorError::Configuration);
        }
        let token =
            std::str::from_utf8(credential.expose()).map_err(|_| ConnectorError::Configuration)?;
        if token.is_empty() {
            return Err(ConnectorError::Configuration);
        }
        let mut authorization = header::HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| ConnectorError::Configuration)?;
        authorization.set_sensitive(true);
        let mut headers = header::HeaderMap::new();
        headers.insert(header::AUTHORIZATION, authorization);
        headers.insert(
            header::ACCEPT,
            header::HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            header::USER_AGENT,
            header::HeaderValue::from_static("eliteforge-agent-connector"),
        );
        headers.insert(
            "x-github-api-version",
            header::HeaderValue::from_static("2022-11-28"),
        );
        let client = Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| ConnectorError::Configuration)?;
        Ok(Arc::new(GitHubConnection { client, base }))
    }
}

struct GitHubConnection {
    client: Client,
    base: Url,
}

#[async_trait]
impl ConnectedProvider for GitHubConnection {
    async fn check(&self) -> Result<(), ConnectorError> {
        let user = self.get_json(self.url(&["user"])?).await?;
        string(&user, "login")?;
        Ok(())
    }

    fn repositories(&self) -> Option<Arc<dyn RepositoryReader>> {
        Some(Arc::new(self.clone()))
    }
    fn tasks(&self) -> Option<Arc<dyn TaskReader>> {
        Some(Arc::new(self.clone()))
    }
    fn task_writer(&self) -> Option<Arc<dyn TaskWriter>> {
        Some(Arc::new(self.clone()))
    }
    fn reviews(&self) -> Option<Arc<dyn ReviewReader>> {
        Some(Arc::new(self.clone()))
    }
    fn review_writer(&self) -> Option<Arc<dyn ReviewWriter>> {
        Some(Arc::new(self.clone()))
    }
}

impl Clone for GitHubConnection {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            base: self.base.clone(),
        }
    }
}

impl GitHubConnection {
    fn url(&self, parts: &[&str]) -> Result<Url, ConnectorError> {
        let mut url = self.base.clone();
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| ConnectorError::Configuration)?;
        segments.pop_if_empty();
        for part in parts {
            segments.push(part);
        }
        drop(segments);
        Ok(url)
    }

    async fn send(
        &self,
        method: Method,
        url: Url,
        body: Option<Value>,
        raw: bool,
    ) -> Result<Response, ConnectorError> {
        let write = method != Method::GET;
        let mut request = self.client.request(method, url);
        if raw {
            request = request.header(header::ACCEPT, "application/vnd.github.raw+json");
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(|_| {
            if write {
                ConnectorError::IndeterminateWrite
            } else {
                ConnectorError::Unavailable
            }
        })?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        if write && status.is_server_error() {
            return Err(ConnectorError::IndeterminateWrite);
        }
        Err(match status.as_u16() {
            401 => ConnectorError::Authentication,
            403 if response
                .headers()
                .get("x-ratelimit-remaining")
                .is_some_and(|v| v == "0") =>
            {
                ConnectorError::RateLimited
            }
            403 => ConnectorError::Forbidden,
            404 => ConnectorError::NotFound,
            429 => ConnectorError::RateLimited,
            400..=499 => ConnectorError::Rejected,
            _ => ConnectorError::Unavailable,
        })
    }

    async fn bytes(mut response: Response) -> Result<Vec<u8>, ConnectorError> {
        let mut result = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| ConnectorError::Unavailable)?
        {
            if result.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(ConnectorError::InvalidResponse);
            }
            result.extend_from_slice(&chunk);
        }
        Ok(result)
    }

    async fn json(response: Response) -> Result<Value, ConnectorError> {
        serde_json::from_slice(&Self::bytes(response).await?)
            .map_err(|_| ConnectorError::InvalidResponse)
    }

    async fn get_json(&self, url: Url) -> Result<Value, ConnectorError> {
        Self::json(self.send(Method::GET, url, None, false).await?).await
    }

    async fn post(&self, url: Url, body: Value) -> Result<Value, ConnectorError> {
        let response = self.send(Method::POST, url, Some(body), false).await?;
        // A successfully accepted write with an unreadable response may have happened.
        Self::json(response)
            .await
            .map_err(|_| ConnectorError::IndeterminateWrite)
    }

    async fn repo_parts(&self, id: &str) -> Result<(String, String), ConnectorError> {
        if id.parse::<u64>().is_err() {
            return Err(ConnectorError::Configuration);
        }
        let repo = self.get_json(self.url(&["repositories", id])?).await?;
        let full = string(&repo, "full_name")?;
        let (owner, name) = full
            .split_once('/')
            .ok_or(ConnectorError::InvalidResponse)?;
        if owner.is_empty() || name.is_empty() || name.contains('/') {
            return Err(ConnectorError::InvalidResponse);
        }
        Ok((owner.into(), name.into()))
    }

    async fn paged(
        &self,
        mut url: Url,
        page: PageRequest,
        items_key: Option<&str>,
    ) -> Result<(Vec<Value>, Option<String>), ConnectorError> {
        let current = page_number(&page)?;
        url.query_pairs_mut()
            .append_pair("page", &current.to_string())
            .append_pair("per_page", &page.limit.to_string());
        let response = self.send(Method::GET, url, None, false).await?;
        let has_next = response
            .headers()
            .get(header::LINK)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.split(',').any(|part| part.contains("rel=\"next\"")));
        let value = Self::json(response).await?;
        let array = match items_key {
            Some(key) => value.get(key),
            None => Some(&value),
        }
        .and_then(Value::as_array)
        .ok_or(ConnectorError::InvalidResponse)?;
        let next = if has_next {
            Some(
                current
                    .checked_add(1)
                    .ok_or(ConnectorError::InvalidResponse)?
                    .to_string(),
            )
        } else {
            None
        };
        Ok((array.clone(), next))
    }

    async fn issue_search(
        &self,
        repository: &str,
        query: &str,
        kind: &str,
        page: PageRequest,
    ) -> Result<Page<Item>, ConnectorError> {
        let (owner, name) = self.repo_parts(repository).await?;
        let mut url = self.url(&["search", "issues"])?;
        url.query_pairs_mut()
            .append_pair("q", &format!("repo:{owner}/{name} is:{kind} {query}"));
        let (items, next_cursor) = self.paged(url, page, Some("items")).await?;
        Ok(Page {
            items: items.iter().map(item).collect::<Result<_, _>>()?,
            next_cursor,
        })
    }

    async fn issue_comments(
        &self,
        repository: &str,
        id: &str,
        page: PageRequest,
    ) -> Result<Page<Comment>, ConnectorError> {
        valid_number(id)?;
        let (owner, name) = self.repo_parts(repository).await?;
        let (items, next_cursor) = self
            .paged(
                self.url(&["repos", &owner, &name, "issues", id, "comments"])?,
                page,
                None,
            )
            .await?;
        Ok(Page {
            items: items.iter().map(comment).collect::<Result<_, _>>()?,
            next_cursor,
        })
    }

    async fn add_comment(
        &self,
        repository: &str,
        id: &str,
        body: &str,
    ) -> Result<Comment, ConnectorError> {
        valid_number(id)?;
        let (owner, name) = self.repo_parts(repository).await?;
        comment(
            &self
                .post(
                    self.url(&["repos", &owner, &name, "issues", id, "comments"])?,
                    json!({"body": body}),
                )
                .await?,
        )
        .map_err(|_| ConnectorError::IndeterminateWrite)
    }
}

fn valid_number(value: &str) -> Result<(), ConnectorError> {
    value
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .map(|_| ())
        .ok_or(ConnectorError::Configuration)
}

fn page_number(page: &PageRequest) -> Result<u32, ConnectorError> {
    if !(1..=100).contains(&page.limit) {
        return Err(ConnectorError::Configuration);
    }
    match &page.cursor {
        Some(cursor) => cursor
            .parse::<u32>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or(ConnectorError::Configuration),
        None => Ok(1),
    }
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, ConnectorError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or(ConnectorError::InvalidResponse)
}

fn repository(value: &Value) -> Result<Repository, ConnectorError> {
    Ok(Repository {
        id: value
            .get("id")
            .and_then(Value::as_u64)
            .ok_or(ConnectorError::InvalidResponse)?
            .to_string(),
        name: string(value, "name")?.into(),
        full_name: string(value, "full_name")?.into(),
        web_url: string(value, "html_url")?.into(),
        clone_url: value
            .get("clone_url")
            .and_then(Value::as_str)
            .map(str::to_owned),
        ssh_url: value
            .get("ssh_url")
            .and_then(Value::as_str)
            .map(str::to_owned),
        default_branch: value
            .get("default_branch")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

fn item(value: &Value) -> Result<Item, ConnectorError> {
    Ok(Item {
        id: value
            .get("number")
            .and_then(Value::as_u64)
            .ok_or(ConnectorError::InvalidResponse)?
            .to_string(),
        title: string(value, "title")?.into(),
        description: value.get("body").and_then(Value::as_str).map(str::to_owned),
        state: string(value, "state")?.into(),
        web_url: string(value, "html_url")?.into(),
    })
}

fn comment(value: &Value) -> Result<Comment, ConnectorError> {
    Ok(Comment {
        id: value
            .get("id")
            .and_then(Value::as_u64)
            .ok_or(ConnectorError::InvalidResponse)?
            .to_string(),
        body: string(value, "body")?.into(),
        author: string(
            value.get("user").ok_or(ConnectorError::InvalidResponse)?,
            "login",
        )?
        .into(),
    })
}

fn content_parts<'a>(
    owner: &'a str,
    name: &'a str,
    path: &'a str,
) -> Result<Vec<&'a str>, ConnectorError> {
    let mut parts = vec!["repos", owner, name, "contents"];
    if !path.is_empty() {
        for segment in path.split('/') {
            if segment.is_empty() || segment == "." || segment == ".." {
                return Err(ConnectorError::Configuration);
            }
            parts.push(segment);
        }
    }
    Ok(parts)
}

#[async_trait]
impl RepositoryReader for GitHubConnection {
    async fn search(
        &self,
        query: &str,
        page: PageRequest,
    ) -> Result<Page<Repository>, ConnectorError> {
        let mut url;
        let key;
        if query.trim().is_empty() {
            url = self.url(&["user", "repos"])?;
            url.query_pairs_mut().append_pair("sort", "updated");
            key = None;
        } else {
            url = self.url(&["search", "repositories"])?;
            url.query_pairs_mut().append_pair("q", query);
            key = Some("items");
        }
        let (items, next_cursor) = self.paged(url, page, key).await?;
        Ok(Page {
            items: items.iter().map(repository).collect::<Result<_, _>>()?,
            next_cursor,
        })
    }

    async fn get(&self, id: &str) -> Result<Repository, ConnectorError> {
        valid_number(id)?;
        repository(&self.get_json(self.url(&["repositories", id])?).await?)
    }

    async fn branches(&self, id: &str, page: PageRequest) -> Result<Page<String>, ConnectorError> {
        let (owner, name) = self.repo_parts(id).await?;
        let (items, next_cursor) = self
            .paged(self.url(&["repos", &owner, &name, "branches"])?, page, None)
            .await?;
        Ok(Page {
            items: items
                .iter()
                .map(|v| string(v, "name").map(str::to_owned))
                .collect::<Result<_, _>>()?,
            next_cursor,
        })
    }

    async fn tree(
        &self,
        id: &str,
        revision: &str,
        path: &str,
        page: PageRequest,
    ) -> Result<Page<TreeEntry>, ConnectorError> {
        let number = page_number(&page)?;
        let (owner, name) = self.repo_parts(id).await?;
        let mut url = self.url(&content_parts(&owner, &name, path)?)?;
        url.query_pairs_mut().append_pair("ref", revision);
        let value = self.get_json(url).await?;
        let entries = value.as_array().ok_or(ConnectorError::InvalidResponse)?;
        // GitHub's contents endpoint has a 1,000-entry limit. Do not silently
        // present a potentially truncated directory as complete.
        if entries.len() >= 1000 {
            return Err(ConnectorError::Unsupported);
        }
        let start = ((number - 1) as usize).saturating_mul(page.limit as usize);
        let end = start.saturating_add(page.limit as usize).min(entries.len());
        let slice = entries
            .get(start.min(entries.len())..end)
            .ok_or(ConnectorError::InvalidResponse)?;
        let items = slice
            .iter()
            .map(|entry| {
                Ok(TreeEntry {
                    path: string(entry, "path")?.into(),
                    is_directory: string(entry, "type")? == "dir",
                })
            })
            .collect::<Result<_, ConnectorError>>()?;
        Ok(Page {
            items,
            next_cursor: if end < entries.len() {
                Some((number + 1).to_string())
            } else {
                None
            },
        })
    }

    async fn read_file(
        &self,
        id: &str,
        revision: &str,
        path: &str,
    ) -> Result<Vec<u8>, ConnectorError> {
        if path.is_empty() {
            return Err(ConnectorError::Configuration);
        }
        let (owner, name) = self.repo_parts(id).await?;
        let mut url = self.url(&content_parts(&owner, &name, path)?)?;
        url.query_pairs_mut().append_pair("ref", revision);
        Self::bytes(self.send(Method::GET, url, None, true).await?).await
    }
}

#[async_trait]
impl TaskReader for GitHubConnection {
    async fn search(
        &self,
        repository: &str,
        query: &str,
        page: PageRequest,
    ) -> Result<Page<Item>, ConnectorError> {
        self.issue_search(repository, query, "issue", page).await
    }

    async fn get(&self, repository: &str, id: &str) -> Result<Item, ConnectorError> {
        valid_number(id)?;
        let (owner, name) = self.repo_parts(repository).await?;
        let value = self
            .get_json(self.url(&["repos", &owner, &name, "issues", id])?)
            .await?;
        if value.get("pull_request").is_some() {
            return Err(ConnectorError::NotFound);
        }
        item(&value)
    }

    async fn comments(
        &self,
        repository: &str,
        id: &str,
        page: PageRequest,
    ) -> Result<Page<Comment>, ConnectorError> {
        self.issue_comments(repository, id, page).await
    }
}

#[async_trait]
impl TaskWriter for GitHubConnection {
    async fn create(
        &self,
        repository: &str,
        title: &str,
        description: &str,
    ) -> Result<Item, ConnectorError> {
        let (owner, name) = self.repo_parts(repository).await?;
        let value = self
            .post(
                self.url(&["repos", &owner, &name, "issues"])?,
                json!({"title": title, "body": description}),
            )
            .await?;
        item(&value).map_err(|_| ConnectorError::IndeterminateWrite)
    }

    async fn comment(
        &self,
        repository: &str,
        id: &str,
        body: &str,
    ) -> Result<Comment, ConnectorError> {
        self.add_comment(repository, id, body).await
    }
}

#[async_trait]
impl ReviewReader for GitHubConnection {
    async fn search(
        &self,
        repository: &str,
        query: &str,
        page: PageRequest,
    ) -> Result<Page<Item>, ConnectorError> {
        self.issue_search(repository, query, "pr", page).await
    }

    async fn get(&self, repository: &str, id: &str) -> Result<Item, ConnectorError> {
        valid_number(id)?;
        let (owner, name) = self.repo_parts(repository).await?;
        item(
            &self
                .get_json(self.url(&["repos", &owner, &name, "pulls", id])?)
                .await?,
        )
    }

    async fn comments(
        &self,
        repository: &str,
        id: &str,
        page: PageRequest,
    ) -> Result<Page<Comment>, ConnectorError> {
        self.issue_comments(repository, id, page).await
    }
}

#[async_trait]
impl ReviewWriter for GitHubConnection {
    async fn create(
        &self,
        repository: &str,
        title: &str,
        description: &str,
        source: &str,
        target: &str,
    ) -> Result<Item, ConnectorError> {
        let (owner, name) = self.repo_parts(repository).await?;
        let value = self
            .post(
                self.url(&["repos", &owner, &name, "pulls"])?,
                json!({"title": title, "body": description, "head": source, "base": target}),
            )
            .await?;
        item(&value).map_err(|_| ConnectorError::IndeterminateWrite)
    }

    async fn comment(
        &self,
        repository: &str,
        id: &str,
        body: &str,
    ) -> Result<Comment, ConnectorError> {
        self.add_comment(repository, id, body).await
    }
}
