//! Gitee.com implementation of the portable connector capabilities.
//! All mutations are sent once and uncertain outcomes are not retried.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use eliteforge_agent_connector::{
    Comment, ConnectedProvider, Connection, ConnectorError, ConnectorProvider, Credential, Item,
    Page, PageRequest, ProviderDescriptor, Repository, RepositoryReader, ReviewReader,
    ReviewWriter, TaskReader, TaskWriter, TreeEntry,
};
use reqwest::{Client, Method, Response, Url, header};
use serde_json::Value;

const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

pub struct GiteeProvider;

#[async_trait]
impl ConnectorProvider for GiteeProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: "gitee".into(),
            display_name: "Gitee".into(),
        }
    }

    async fn connect(
        &self,
        connection: &Connection,
        credential: Credential,
    ) -> Result<Arc<dyn ConnectedProvider>, ConnectorError> {
        let mut base =
            Url::parse(&connection.endpoint).map_err(|_| ConnectorError::Configuration)?;
        if base.as_str().trim_end_matches('/') != "https://gitee.com" {
            return Err(ConnectorError::Configuration);
        }
        base.path_segments_mut()
            .map_err(|_| ConnectorError::Configuration)?
            .pop_if_empty()
            .push("api")
            .push("v5");
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
            header::HeaderValue::from_static("application/json"),
        );
        headers.insert(
            header::USER_AGENT,
            header::HeaderValue::from_static("eliteforge-agent-connector"),
        );
        let client = Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| ConnectorError::Configuration)?;
        Ok(Arc::new(GiteeConnection { client, base }))
    }
}

#[derive(Clone)]
struct GiteeConnection {
    client: Client,
    base: Url,
}

#[async_trait]
impl ConnectedProvider for GiteeConnection {
    async fn check(&self) -> Result<(), ConnectorError> {
        string(&self.get(self.url(&["user"])?).await?, "login")?;
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

impl GiteeConnection {
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
        form: Option<&[(&str, &str)]>,
    ) -> Result<Response, ConnectorError> {
        let write = method != Method::GET;
        let mut request = self.client.request(method, url);
        if let Some(form) = form {
            request = request.form(form);
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
            403 => ConnectorError::Forbidden,
            404 => ConnectorError::NotFound,
            429 => ConnectorError::RateLimited,
            400..=499 => ConnectorError::Rejected,
            _ => ConnectorError::Unavailable,
        })
    }

    async fn json(mut response: Response) -> Result<Value, ConnectorError> {
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| ConnectorError::Unavailable)?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(ConnectorError::InvalidResponse);
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| ConnectorError::InvalidResponse)
    }

    async fn get(&self, url: Url) -> Result<Value, ConnectorError> {
        Self::json(self.send(Method::GET, url, None).await?).await
    }

    async fn post(&self, url: Url, form: &[(&str, &str)]) -> Result<Value, ConnectorError> {
        let response = self.send(Method::POST, url, Some(form)).await?;
        Self::json(response)
            .await
            .map_err(|_| ConnectorError::IndeterminateWrite)
    }

    async fn paged(
        &self,
        mut url: Url,
        page: PageRequest,
    ) -> Result<(Vec<Value>, Option<String>), ConnectorError> {
        let current = page_number(&page)?;
        url.query_pairs_mut()
            .append_pair("page", &current.to_string())
            .append_pair("per_page", &page.limit.to_string());
        let value = self.get(url).await?;
        let items = value
            .as_array()
            .ok_or(ConnectorError::InvalidResponse)?
            .clone();
        // The API does not consistently expose pagination headers.
        let next = if items.len() == page.limit as usize {
            Some(
                current
                    .checked_add(1)
                    .ok_or(ConnectorError::InvalidResponse)?
                    .to_string(),
            )
        } else {
            None
        };
        Ok((items, next))
    }

    async fn list_items(
        &self,
        repository: &str,
        kind: &str,
        query: &str,
        page: PageRequest,
    ) -> Result<Page<Item>, ConnectorError> {
        let (owner, name) = repo_parts(repository)?;
        let mut url = self.url(&["repos", owner, name, kind])?;
        url.query_pairs_mut().append_pair("state", "all");
        if kind == "issues" {
            url.query_pairs_mut().append_pair("q", query);
        }
        let (entries, next_cursor) = self.paged(url, page).await?;
        let items = entries.iter().map(item).collect::<Result<Vec<_>, _>>()?;
        let items = if kind == "pulls" && !query.trim().is_empty() {
            let needle = query.to_lowercase();
            items
                .into_iter()
                .filter(|item| {
                    item.title.to_lowercase().contains(&needle)
                        || item
                            .description
                            .as_deref()
                            .unwrap_or("")
                            .to_lowercase()
                            .contains(&needle)
                })
                .collect()
        } else {
            items
        };
        Ok(Page { items, next_cursor })
    }

    async fn get_item(
        &self,
        repository: &str,
        kind: &str,
        id: &str,
    ) -> Result<Item, ConnectorError> {
        let (owner, name) = repo_parts(repository)?;
        valid_item_id(id)?;
        item(
            &self
                .get(self.url(&["repos", owner, name, kind, id])?)
                .await?,
        )
    }

    async fn comments(
        &self,
        repository: &str,
        kind: &str,
        id: &str,
        page: PageRequest,
    ) -> Result<Page<Comment>, ConnectorError> {
        let (owner, name) = repo_parts(repository)?;
        valid_item_id(id)?;
        let (entries, next_cursor) = self
            .paged(
                self.url(&["repos", owner, name, kind, id, "comments"])?,
                page,
            )
            .await?;
        Ok(Page {
            items: entries.iter().map(comment).collect::<Result<_, _>>()?,
            next_cursor,
        })
    }

    async fn add_comment(
        &self,
        repository: &str,
        kind: &str,
        id: &str,
        body: &str,
    ) -> Result<Comment, ConnectorError> {
        let (owner, name) = repo_parts(repository)?;
        valid_item_id(id)?;
        let value = self
            .post(
                self.url(&["repos", owner, name, kind, id, "comments"])?,
                &[("body", body)],
            )
            .await?;
        comment(&value).map_err(|_| ConnectorError::IndeterminateWrite)
    }
}

fn repo_parts(value: &str) -> Result<(&str, &str), ConnectorError> {
    let (owner, name) = value.split_once('/').ok_or(ConnectorError::Configuration)?;
    if owner.is_empty()
        || name.is_empty()
        || name.contains('/')
        || owner == "."
        || owner == ".."
        || name == "."
        || name == ".."
    {
        return Err(ConnectorError::Configuration);
    }
    Ok((owner, name))
}

fn valid_item_id(value: &str) -> Result<(), ConnectorError> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(ConnectorError::Configuration);
    }
    Ok(())
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
    let full_name = string(value, "full_name")?;
    repo_parts(full_name)?;
    Ok(Repository {
        id: full_name.into(),
        name: string(value, "name")?.into(),
        full_name: full_name.into(),
        web_url: string(value, "html_url")?.into(),
        clone_url: value
            .get("html_url")
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
    let number = value.get("number").ok_or(ConnectorError::InvalidResponse)?;
    let id = match number {
        Value::String(id) => id.clone(),
        Value::Number(id) => id.to_string(),
        _ => return Err(ConnectorError::InvalidResponse),
    };
    Ok(Item {
        id,
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
impl RepositoryReader for GiteeConnection {
    async fn search(
        &self,
        query: &str,
        page: PageRequest,
    ) -> Result<Page<Repository>, ConnectorError> {
        let mut url = self.url(&["user", "repos"])?;
        url.query_pairs_mut()
            .append_pair("q", query)
            .append_pair("sort", "updated");
        let (entries, next_cursor) = self.paged(url, page).await?;
        Ok(Page {
            items: entries.iter().map(repository).collect::<Result<_, _>>()?,
            next_cursor,
        })
    }
    async fn get(&self, id: &str) -> Result<Repository, ConnectorError> {
        let (owner, name) = repo_parts(id)?;
        repository(&self.get(self.url(&["repos", owner, name])?).await?)
    }
    async fn branches(&self, id: &str, page: PageRequest) -> Result<Page<String>, ConnectorError> {
        let (owner, name) = repo_parts(id)?;
        let (entries, next_cursor) = self
            .paged(self.url(&["repos", owner, name, "branches"])?, page)
            .await?;
        Ok(Page {
            items: entries
                .iter()
                .map(|entry| string(entry, "name").map(str::to_owned))
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
        let (owner, name) = repo_parts(id)?;
        let mut url = self.url(&content_parts(owner, name, path)?)?;
        url.query_pairs_mut().append_pair("ref", revision);
        let value = self.get(url).await?;
        let entries = value.as_array().ok_or(ConnectorError::InvalidResponse)?;
        let start = ((number - 1) as usize)
            .saturating_mul(page.limit as usize)
            .min(entries.len());
        let end = start.saturating_add(page.limit as usize).min(entries.len());
        let items = entries[start..end]
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
                Some(
                    number
                        .checked_add(1)
                        .ok_or(ConnectorError::InvalidResponse)?
                        .to_string(),
                )
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
        let (owner, name) = repo_parts(id)?;
        let mut url = self.url(&content_parts(owner, name, path)?)?;
        url.query_pairs_mut().append_pair("ref", revision);
        let value = self.get(url).await?;
        if string(&value, "encoding")? != "base64" {
            return Err(ConnectorError::Unsupported);
        }
        let content: String = string(&value, "content")?
            .chars()
            .filter(|c| !c.is_ascii_whitespace())
            .collect();
        STANDARD
            .decode(content)
            .map_err(|_| ConnectorError::InvalidResponse)
    }
}

#[async_trait]
impl TaskReader for GiteeConnection {
    async fn search(
        &self,
        repository: &str,
        query: &str,
        page: PageRequest,
    ) -> Result<Page<Item>, ConnectorError> {
        self.list_items(repository, "issues", query, page).await
    }
    async fn get(&self, repository: &str, id: &str) -> Result<Item, ConnectorError> {
        self.get_item(repository, "issues", id).await
    }
    async fn comments(
        &self,
        repository: &str,
        id: &str,
        page: PageRequest,
    ) -> Result<Page<Comment>, ConnectorError> {
        self.comments(repository, "issues", id, page).await
    }
}

#[async_trait]
impl TaskWriter for GiteeConnection {
    async fn create(
        &self,
        repository: &str,
        title: &str,
        description: &str,
    ) -> Result<Item, ConnectorError> {
        let (owner, name) = repo_parts(repository)?;
        let value = self
            .post(
                self.url(&["repos", owner, "issues"])?,
                &[("repo", name), ("title", title), ("body", description)],
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
        self.add_comment(repository, "issues", id, body).await
    }
}

#[async_trait]
impl ReviewReader for GiteeConnection {
    async fn search(
        &self,
        repository: &str,
        query: &str,
        page: PageRequest,
    ) -> Result<Page<Item>, ConnectorError> {
        self.list_items(repository, "pulls", query, page).await
    }
    async fn get(&self, repository: &str, id: &str) -> Result<Item, ConnectorError> {
        self.get_item(repository, "pulls", id).await
    }
    async fn comments(
        &self,
        repository: &str,
        id: &str,
        page: PageRequest,
    ) -> Result<Page<Comment>, ConnectorError> {
        self.comments(repository, "pulls", id, page).await
    }
}

#[async_trait]
impl ReviewWriter for GiteeConnection {
    async fn create(
        &self,
        repository: &str,
        title: &str,
        description: &str,
        source: &str,
        target: &str,
    ) -> Result<Item, ConnectorError> {
        let (owner, name) = repo_parts(repository)?;
        let value = self
            .post(
                self.url(&["repos", owner, name, "pulls"])?,
                &[
                    ("title", title),
                    ("body", description),
                    ("head", source),
                    ("base", target),
                ],
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
        self.add_comment(repository, "pulls", id, body).await
    }
}
