//! GitLab.com and self-managed GitLab connector.
//!
//! Credentials are never logged and redirects are disabled. A write is never
//! replayed after a transport failure or a server-side error.

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

pub struct GitLabProvider;

#[async_trait]
impl ConnectorProvider for GitLabProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: "gitlab".into(),
            display_name: "GitLab".into(),
        }
    }

    async fn connect(
        &self,
        connection: &Connection,
        credential: Credential,
    ) -> Result<Arc<dyn ConnectedProvider>, ConnectorError> {
        let mut base =
            Url::parse(&connection.endpoint).map_err(|_| ConnectorError::Configuration)?;
        if !matches!(base.scheme(), "http" | "https")
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(ConnectorError::Configuration);
        }
        {
            let mut segments = base
                .path_segments_mut()
                .map_err(|_| ConnectorError::Configuration)?;
            segments.pop_if_empty().push("api").push("v4");
        }
        let mut token = header::HeaderValue::from_bytes(credential.expose())
            .map_err(|_| ConnectorError::Configuration)?;
        if token.is_empty() {
            return Err(ConnectorError::Configuration);
        }
        token.set_sensitive(true);
        let mut headers = header::HeaderMap::new();
        headers.insert("private-token", token);
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
        Ok(Arc::new(GitLabConnection { client, base }))
    }
}

#[derive(Clone)]
struct GitLabConnection {
    client: Client,
    base: Url,
}

#[async_trait]
impl ConnectedProvider for GitLabConnection {
    async fn check(&self) -> Result<(), ConnectorError> {
        let user = self.get(self.url(&["user"])?).await?;
        string(&user, "username")?;
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

impl GitLabConnection {
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
    ) -> Result<Response, ConnectorError> {
        let write = method != Method::GET;
        let mut request = self.client.request(method, url);
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
            403 => ConnectorError::Forbidden,
            404 => ConnectorError::NotFound,
            429 => ConnectorError::RateLimited,
            400..=499 => ConnectorError::Rejected,
            _ => ConnectorError::Unavailable,
        })
    }

    async fn bytes(mut response: Response) -> Result<Vec<u8>, ConnectorError> {
        let mut output = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| ConnectorError::Unavailable)?
        {
            if output.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(ConnectorError::InvalidResponse);
            }
            output.extend_from_slice(&chunk);
        }
        Ok(output)
    }

    async fn json(response: Response) -> Result<Value, ConnectorError> {
        serde_json::from_slice(&Self::bytes(response).await?)
            .map_err(|_| ConnectorError::InvalidResponse)
    }

    async fn get(&self, url: Url) -> Result<Value, ConnectorError> {
        Self::json(self.send(Method::GET, url, None).await?).await
    }

    async fn post(&self, url: Url, body: Value) -> Result<Value, ConnectorError> {
        let response = self.send(Method::POST, url, Some(body)).await?;
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
        let response = self.send(Method::GET, url, None).await?;
        let has_next_header = response.headers().contains_key("x-next-page");
        let next = response
            .headers()
            .get("x-next-page")
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        if let Some(ref cursor) = next
            && cursor
                .parse::<u32>()
                .ok()
                .filter(|n| *n > current)
                .is_none()
        {
            return Err(ConnectorError::InvalidResponse);
        }
        let value = Self::json(response).await?;
        let items = value
            .as_array()
            .ok_or(ConnectorError::InvalidResponse)?
            .clone();
        // Some installations omit pagination headers; a full page requires a
        // further request to avoid silently truncating results.
        let next = if !has_next_header && items.len() == page.limit as usize {
            Some(
                current
                    .checked_add(1)
                    .ok_or(ConnectorError::InvalidResponse)?
                    .to_string(),
            )
        } else {
            next
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
        valid_number(repository)?;
        let mut url = self.url(&["projects", repository, kind])?;
        url.query_pairs_mut()
            .append_pair("scope", "all")
            .append_pair("search", query);
        let (entries, next_cursor) = self.paged(url, page).await?;
        Ok(Page {
            items: entries.iter().map(item).collect::<Result<_, _>>()?,
            next_cursor,
        })
    }

    async fn get_item(
        &self,
        repository: &str,
        kind: &str,
        id: &str,
    ) -> Result<Item, ConnectorError> {
        valid_number(repository)?;
        valid_number(id)?;
        item(
            &self
                .get(self.url(&["projects", repository, kind, id])?)
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
        valid_number(repository)?;
        valid_number(id)?;
        let (entries, next_cursor) = self
            .paged(
                self.url(&["projects", repository, kind, id, "notes"])?,
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
        valid_number(repository)?;
        valid_number(id)?;
        let value = self
            .post(
                self.url(&["projects", repository, kind, id, "notes"])?,
                json!({"body": body}),
            )
            .await?;
        comment(&value).map_err(|_| ConnectorError::IndeterminateWrite)
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
        full_name: string(value, "path_with_namespace")?.into(),
        web_url: string(value, "web_url")?.into(),
        clone_url: value
            .get("http_url_to_repo")
            .and_then(Value::as_str)
            .map(str::to_owned),
        ssh_url: value
            .get("ssh_url_to_repo")
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
            .get("iid")
            .and_then(Value::as_u64)
            .ok_or(ConnectorError::InvalidResponse)?
            .to_string(),
        title: string(value, "title")?.into(),
        description: value
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned),
        state: string(value, "state")?.into(),
        web_url: string(value, "web_url")?.into(),
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
            value.get("author").ok_or(ConnectorError::InvalidResponse)?,
            "username",
        )?
        .into(),
    })
}

#[async_trait]
impl RepositoryReader for GitLabConnection {
    async fn search(
        &self,
        query: &str,
        page: PageRequest,
    ) -> Result<Page<Repository>, ConnectorError> {
        let mut url = self.url(&["projects"])?;
        url.query_pairs_mut()
            .append_pair("order_by", "last_activity_at")
            .append_pair("search", query);
        let (entries, next_cursor) = self.paged(url, page).await?;
        Ok(Page {
            items: entries.iter().map(repository).collect::<Result<_, _>>()?,
            next_cursor,
        })
    }

    async fn get(&self, id: &str) -> Result<Repository, ConnectorError> {
        valid_number(id)?;
        repository(&self.get(self.url(&["projects", id])?).await?)
    }

    async fn branches(&self, id: &str, page: PageRequest) -> Result<Page<String>, ConnectorError> {
        valid_number(id)?;
        let (entries, next_cursor) = self
            .paged(self.url(&["projects", id, "repository", "branches"])?, page)
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
        valid_number(id)?;
        let mut url = self.url(&["projects", id, "repository", "tree"])?;
        url.query_pairs_mut()
            .append_pair("ref", revision)
            .append_pair("path", path);
        let (entries, next_cursor) = self.paged(url, page).await?;
        Ok(Page {
            items: entries
                .iter()
                .map(|entry| {
                    Ok(TreeEntry {
                        path: string(entry, "path")?.into(),
                        is_directory: string(entry, "type")? == "tree",
                    })
                })
                .collect::<Result<_, ConnectorError>>()?,
            next_cursor,
        })
    }

    async fn read_file(
        &self,
        id: &str,
        revision: &str,
        path: &str,
    ) -> Result<Vec<u8>, ConnectorError> {
        valid_number(id)?;
        if path.is_empty() {
            return Err(ConnectorError::Configuration);
        }
        let mut url = self.url(&["projects", id, "repository", "files", path, "raw"])?;
        url.query_pairs_mut().append_pair("ref", revision);
        Self::bytes(self.send(Method::GET, url, None).await?).await
    }
}

#[async_trait]
impl TaskReader for GitLabConnection {
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
impl TaskWriter for GitLabConnection {
    async fn create(
        &self,
        repository: &str,
        title: &str,
        description: &str,
    ) -> Result<Item, ConnectorError> {
        valid_number(repository)?;
        let value = self
            .post(
                self.url(&["projects", repository, "issues"])?,
                json!({"title": title, "description": description}),
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
impl ReviewReader for GitLabConnection {
    async fn search(
        &self,
        repository: &str,
        query: &str,
        page: PageRequest,
    ) -> Result<Page<Item>, ConnectorError> {
        self.list_items(repository, "merge_requests", query, page)
            .await
    }
    async fn get(&self, repository: &str, id: &str) -> Result<Item, ConnectorError> {
        self.get_item(repository, "merge_requests", id).await
    }
    async fn comments(
        &self,
        repository: &str,
        id: &str,
        page: PageRequest,
    ) -> Result<Page<Comment>, ConnectorError> {
        self.comments(repository, "merge_requests", id, page).await
    }
}

#[async_trait]
impl ReviewWriter for GitLabConnection {
    async fn create(
        &self,
        repository: &str,
        title: &str,
        description: &str,
        source: &str,
        target: &str,
    ) -> Result<Item, ConnectorError> {
        valid_number(repository)?;
        let value = self.post(self.url(&["projects", repository, "merge_requests"])?, json!({"title": title, "description": description, "source_branch": source, "target_branch": target})).await?;
        item(&value).map_err(|_| ConnectorError::IndeterminateWrite)
    }
    async fn comment(
        &self,
        repository: &str,
        id: &str,
        body: &str,
    ) -> Result<Comment, ConnectorError> {
        self.add_comment(repository, "merge_requests", id, body)
            .await
    }
}
