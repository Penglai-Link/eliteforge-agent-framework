//! Per-account discovery with independent domain updates.
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crab_serverapi::{
    base::AuthorizedClientApi,
    gateway::{ChatModelInfo, ProtocolType, QualityType},
    multimedia::{MediaType, MultimediaModelInfo},
};
use eliteforge_agent_core::{ModelCatalogSnapshot, ModelCatalogStore};
use eliteforge_agent_framework::ModelRuntime;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Notify, RwLock, watch};
use tokio_util::sync::CancellationToken;

use crate::{
    AccountContext, PersonAiConfig, PersonAiContextProvider, PersonAiModels, PersonAiServices,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredChatModel {
    /// Full provider-qualified code; never split a model name on `/`.
    pub id: String,
    pub model_code: String,
    pub provider_code: Option<String>,
    pub model_name: Option<String>,
    pub provider_name: Option<String>,
    pub protocols: Vec<String>,
    pub quality_tiers: BTreeSet<String>,
    pub context_length: Option<i32>,
    pub image_understanding: bool,
    pub video_understanding: bool,
}

impl DiscoveredChatModel {
    fn from_sdk(model: ChatModelInfo, tier: QualityType) -> Self {
        let id = match model.provider_code.as_deref() {
            Some(provider) if !model.model_code.starts_with(&format!("{provider}/")) => {
                format!("{provider}/{}", model.model_code)
            }
            _ => model.model_code.clone(),
        };
        let mut protocols = Vec::new();
        if model.supports_protocol(ProtocolType::ChatCompletions) {
            protocols.push("chat_completions".into());
        }
        if model.supports_protocol(ProtocolType::Responses) {
            protocols.push("responses".into());
        }
        Self {
            id,
            model_code: model.model_code,
            provider_code: model.provider_code,
            model_name: model.model_name,
            provider_name: model.provider_name,
            protocols,
            quality_tiers: BTreeSet::from([tier.as_str().to_string()]),
            context_length: model.context_length,
            image_understanding: model.image_understanding.unwrap_or(false),
            video_understanding: model.video_understanding.unwrap_or(false),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredMediaModel {
    pub id: String,
    pub model_code: String,
    pub model_name: Option<String>,
    pub media_type: i32,
}

impl From<MultimediaModelInfo> for DiscoveredMediaModel {
    fn from(value: MultimediaModelInfo) -> Self {
        Self {
            id: value.model_id,
            model_code: value.model_code,
            model_name: value.model_name,
            media_type: value.media_type,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersonAiDiscoverySnapshot {
    pub account_scope: String,
    pub account_revision: u64,
    pub synced_at_unix_ms: u64,
    pub chat: Vec<DiscoveredChatModel>,
    pub media: BTreeMap<i32, Vec<DiscoveredMediaModel>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncDomainStatus {
    pub last_success_unix_ms: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersonAiSyncStatus {
    pub account_scope: Option<String>,
    pub domains: BTreeMap<String, SyncDomainStatus>,
    pub stale: bool,
    pub cache_degraded: bool,
}

struct SyncState {
    snapshot: Option<Arc<PersonAiDiscoverySnapshot>>,
    status: PersonAiSyncStatus,
}

pub struct PersonAiModelSync {
    config: PersonAiConfig,
    api: AuthorizedClientApi,
    _services: Arc<PersonAiServices>,
    context: Arc<dyn PersonAiContextProvider>,
    models: Arc<PersonAiModels>,
    runtime: Arc<ModelRuntime>,
    store: RwLock<Option<Arc<dyn ModelCatalogStore>>>,
    state: RwLock<SyncState>,
    lock: Mutex<()>,
    refresh: Notify,
    changes: watch::Sender<Option<Arc<PersonAiDiscoverySnapshot>>>,
}

impl PersonAiModelSync {
    pub fn new(
        config: PersonAiConfig,
        services: Arc<PersonAiServices>,
        context: Arc<dyn PersonAiContextProvider>,
        models: Arc<PersonAiModels>,
        runtime: Arc<ModelRuntime>,
    ) -> Self {
        let (changes, _) = watch::channel(None);
        Self {
            api: AuthorizedClientApi::new(&config.server_base_url)
                .expect("PersonAiServices already validated the same URL"),
            config,
            _services: services,
            context,
            models,
            runtime,
            store: RwLock::new(None),
            state: RwLock::new(SyncState {
                snapshot: None,
                status: PersonAiSyncStatus::default(),
            }),
            lock: Mutex::new(()),
            refresh: Notify::new(),
            changes,
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<Option<Arc<PersonAiDiscoverySnapshot>>> {
        self.changes.subscribe()
    }

    pub async fn set_store(&self, store: Option<Arc<dyn ModelCatalogStore>>) {
        *self.store.write().await = store;
    }

    pub async fn snapshot(&self) -> Option<Arc<PersonAiDiscoverySnapshot>> {
        self.state.read().await.snapshot.clone()
    }

    pub async fn status(&self) -> PersonAiSyncStatus {
        self.state.read().await.status.clone()
    }

    pub fn request_refresh(&self) {
        self.refresh.notify_one();
    }

    pub fn start(self: &Arc<Self>, stop: CancellationToken) -> tokio::task::JoinHandle<()> {
        let sync = self.clone();
        tokio::spawn(async move {
            let mut context_updates = sync.context.subscribe();
            let interval = sync.config.sync_interval.max(Duration::from_secs(1));
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = ticker.tick() => {},
                    _ = sync.refresh.notified() => {},
                    changed = context_updates.changed() => {
                        if changed.is_err() { break; }
                    },
                }
                if stop.is_cancelled() {
                    break;
                }
                tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = sync.refresh_now() => {},
                }
            }
        })
    }

    /// Refresh domains independently. A missing or null `data` field is an error;
    /// a genuine `data: []` removes that domain's previous entries.
    pub async fn refresh_now(&self) -> Result<PersonAiSyncStatus, String> {
        let _guard = self.lock.lock().await;
        let Some(account) = self.context.current() else {
            self.runtime
                .replace_connection(
                    &self.config.connection_id,
                    eliteforge_agent_framework::ModelRuntimeUpdate::new("", 0, vec![]),
                )
                .map_err(|error| error.to_string())?;
            let mut state = self.state.write().await;
            state.snapshot = None;
            state.status = PersonAiSyncStatus::default();
            self.changes.send_replace(None);
            return Ok(state.status.clone());
        };
        let previous = self
            .state
            .read()
            .await
            .snapshot
            .clone()
            .filter(|old| old.account_scope == account.account_scope);
        let mut restored = None;
        if previous
            .as_ref()
            .is_none_or(|old| old.account_revision != account.revision)
        {
            self.runtime
                .replace_connection(
                    &self.config.connection_id,
                    eliteforge_agent_framework::ModelRuntimeUpdate::new(
                        &account.account_scope,
                        account.revision,
                        vec![],
                    ),
                )
                .map_err(|error| error.to_string())?;
            if let Some(previous) = &previous {
                let mut current = (**previous).clone();
                current.account_revision = account.revision;
                if self.is_current(&account) {
                    self.publish(&account, &current).await?;
                    restored = Some(current);
                }
            } else if let Some(store) = self.store.read().await.clone() {
                if let Ok(Some(cached)) = store.load(&self.cache_key(&account.account_scope)).await
                {
                    if let Some(mut cached_snapshot) = Self::restore_snapshot(&cached) {
                        cached_snapshot.account_scope = account.account_scope.clone();
                        cached_snapshot.account_revision = account.revision;
                        if self.is_current(&account) {
                            self.publish(&account, &cached_snapshot).await?;
                            restored = Some(cached_snapshot);
                        }
                    }
                }
            }
        }
        let mut snapshot = restored
            .or_else(|| previous.as_deref().cloned())
            .unwrap_or_default();
        snapshot.account_scope = account.account_scope.clone();
        snapshot.account_revision = account.revision;
        let mut status = self.state.read().await.status.clone();
        if status.account_scope.as_deref() != Some(&account.account_scope) {
            status = PersonAiSyncStatus::default();
        }
        status.account_scope = Some(account.account_scope.clone());
        let now = unix_ms();
        match self.fetch_chat(&account).await {
            Ok(models) => {
                snapshot.chat = models;
                status.domains.insert(
                    "chat".into(),
                    SyncDomainStatus {
                        last_success_unix_ms: Some(now),
                        last_error: None,
                    },
                );
            }
            Err(error) => mark_error(&mut status, "chat", error),
        }
        for media_type in [
            MediaType::IMAGE,
            MediaType::VIDEO,
            MediaType::SPEECH,
            MediaType::AIGC_TEXT_DETECTION,
            MediaType::AIGC_IMAGE_DETECTION,
        ] {
            let domain = format!("media:{media_type}");
            match self.fetch_media(&account, media_type).await {
                Ok(models) => {
                    snapshot.media.insert(media_type, models);
                    status.domains.insert(
                        domain,
                        SyncDomainStatus {
                            last_success_unix_ms: Some(now),
                            last_error: None,
                        },
                    );
                }
                Err(error) => mark_error(&mut status, &domain, error),
            }
        }
        // A token update, logout, or account switch invalidates this response.
        if !self.is_current(&account) {
            return Err("PersonAI account context changed during synchronization".into());
        }
        snapshot.synced_at_unix_ms = now;
        status.stale = status
            .domains
            .values()
            .any(|domain| domain.last_error.is_some());
        self.publish(&account, &snapshot).await?;
        if let Some(store) = self.store.read().await.clone() {
            let cached = self.persist_snapshot(&snapshot);
            if let Err(error) = store.save(&cached).await {
                status.domains.entry("cache".into()).or_default().last_error =
                    Some(error.to_string());
                status.cache_degraded = true;
            } else {
                status.cache_degraded = false;
                status.domains.remove("cache");
            }
        }
        let snapshot = Arc::new(snapshot);
        let mut state = self.state.write().await;
        state.snapshot = Some(snapshot.clone());
        state.status = status.clone();
        self.changes.send_replace(Some(snapshot));
        Ok(status)
    }

    fn is_current(&self, expected: &Arc<AccountContext>) -> bool {
        self.context.current().is_some_and(|current| {
            Arc::ptr_eq(&current, expected)
                && current.account_scope == expected.account_scope
                && current.revision == expected.revision
        })
    }

    async fn publish(
        &self,
        account: &Arc<AccountContext>,
        snapshot: &PersonAiDiscoverySnapshot,
    ) -> Result<(), String> {
        let update = self
            .models
            .runtime_update(account.clone(), snapshot, &self.config)
            .map_err(|error| error.to_string())?;
        let replaced = self
            .runtime
            .replace_connection_if_current(
                &self.config.connection_id,
                &account.account_scope,
                account.revision,
                update,
            )
            .map_err(|error| error.to_string())?;
        if replaced {
            Ok(())
        } else {
            Err("PersonAI model update superseded by another account context".into())
        }
    }

    fn persist_snapshot(&self, snapshot: &PersonAiDiscoverySnapshot) -> ModelCatalogSnapshot {
        let mut cached = self.models.catalog_snapshot(snapshot, &self.config);
        cached.account_scope = self.cache_key(&snapshot.account_scope);
        cached
    }

    fn cache_key(&self, account_scope: &str) -> String {
        format!("personai:{}:{account_scope}", self.config.connection_id)
    }

    fn restore_snapshot(cached: &ModelCatalogSnapshot) -> Option<PersonAiDiscoverySnapshot> {
        crate::models::restore_discovery(cached)
    }

    async fn fetch_chat(
        &self,
        account: &AccountContext,
    ) -> Result<Vec<DiscoveredChatModel>, String> {
        let mut merged = BTreeMap::<String, DiscoveredChatModel>::new();
        for tier in [QualityType::Low, QualityType::Middle, QualityType::High] {
            let request = self
                .api
                .new_authorized_request(Method::GET, "/chat/models", &account.authorization)
                .map_err(|error| error.to_string())?
                .query(&[("quality_type", tier.as_str())]);
            let models = self
                .api
                .execute_for_object::<Vec<ChatModelInfo>>(
                    request,
                    Some(&account.authorization),
                    "PersonAI Chat model sync",
                )
                .await
                .map_err(|error| error.to_string())?;
            for info in models {
                let item = DiscoveredChatModel::from_sdk(info, tier);
                merged
                    .entry(item.id.clone())
                    .and_modify(|prior| {
                        prior.quality_tiers.extend(item.quality_tiers.clone());
                    })
                    .or_insert(item);
            }
        }
        Ok(merged.into_values().collect())
    }

    async fn fetch_media(
        &self,
        account: &AccountContext,
        media_type: i32,
    ) -> Result<Vec<DiscoveredMediaModel>, String> {
        let request = self
            .api
            .new_authorized_request(Method::GET, "/multimedia/models", &account.authorization)
            .map_err(|error| error.to_string())?
            .query(&[("media_type", media_type)]);
        self.api
            .execute_for_object::<Vec<MultimediaModelInfo>>(
                request,
                Some(&account.authorization),
                "PersonAI multimedia model sync",
            )
            .await
            .map(|items| {
                items
                    .into_iter()
                    .filter(|item| item.media_type == media_type)
                    .map(Into::into)
                    .collect()
            })
            .map_err(|error| error.to_string())
    }
}

fn mark_error(status: &mut PersonAiSyncStatus, domain: &str, error: String) {
    status.domains.entry(domain.into()).or_default().last_error = Some(error);
    status.stale = true;
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crab_serverapi::base::AuthorizedClientContext;
    use eliteforge_agent_core::{
        ChatModel, ChatRequest, ChatResponse, ChatStream, ModelConnection, ModelDescriptor,
        ModelError, ModelJobStatus, ModelRef, VideoGenerationModel, VideoJob,
    };
    use eliteforge_agent_framework::{ModelRuntime, ModelRuntimeUpdate};
    use std::sync::{
        Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn keeps_provider_qualified_model_code_intact() {
        let model: ChatModelInfo = serde_json::from_value(serde_json::json!({
            "provider_code": "kuaishou", "model_code": "kuaishou/deepseek"
        }))
        .unwrap();
        assert_eq!(
            DiscoveredChatModel::from_sdk(model, QualityType::Low).id,
            "kuaishou/deepseek"
        );
    }

    #[test]
    fn returns_distinct_code_for_same_model_at_different_providers() {
        let a: ChatModelInfo = serde_json::from_value(serde_json::json!({
            "provider_code": "a", "model_code": "shared"
        }))
        .unwrap();
        let b: ChatModelInfo = serde_json::from_value(serde_json::json!({
            "provider_code": "b", "model_code": "shared"
        }))
        .unwrap();
        assert_ne!(
            DiscoveredChatModel::from_sdk(a, QualityType::Low).id,
            DiscoveredChatModel::from_sdk(b, QualityType::Low).id
        );
    }

    async fn mock_envelope(body: &'static str) -> String {
        let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = server.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = server.accept().await.unwrap();
            let mut buffer = [0_u8; 4096];
            let _ = socket.read(&mut buffer).await.unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        format!("http://{address}")
    }

    async fn read_models(body: &'static str) -> crab_serverapi::error::Result<Vec<ChatModelInfo>> {
        let url = mock_envelope(body).await;
        let api = AuthorizedClientApi::new(url)?;
        let context = AuthorizedClientContext::new()
            .with_token("test")
            .with_product_code("test")
            .with_product_version("1.0.0");
        let request = api.new_authorized_request(Method::GET, "/chat/models", &context)?;
        api.execute_for_object::<Vec<ChatModelInfo>>(request, Some(&context), "mock sync")
            .await
    }

    #[tokio::test]
    async fn explicit_empty_list_removes_models_but_null_is_an_error() {
        assert!(
            read_models(r#"{"code":0,"data":[]}"#)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(read_models(r#"{"code":0,"data":null}"#).await.is_err());
    }

    struct MockDiscovery {
        url: String,
        chat_low: Arc<StdMutex<String>>,
        video: Arc<StdMutex<String>>,
        hold_low: Arc<AtomicBool>,
        reached_low: Arc<Notify>,
        release_low: Arc<Notify>,
        task: tokio::task::JoinHandle<()>,
    }

    struct OtherProvider;
    #[async_trait::async_trait]
    impl ChatModel for OtherProvider {
        async fn generate(
            &self,
            _model: &str,
            _request: ChatRequest,
            _stream: &dyn ChatStream,
        ) -> Result<ChatResponse, ModelError> {
            Ok(ChatResponse::text("other"))
        }
    }

    impl MockDiscovery {
        async fn start() -> Self {
            let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", server.local_addr().unwrap());
            let chat_low = Arc::new(StdMutex::new(String::from(
                r#"[{"model_code":"old","provider_code":"p","context_length":8192}]"#,
            )));
            let video = Arc::new(StdMutex::new(String::from(
                r#"[{"model_id":"v1","model_code":"video","media_type":1}]"#,
            )));
            let low = chat_low.clone();
            let videos = video.clone();
            let hold_low = Arc::new(AtomicBool::new(false));
            let reached_low = Arc::new(Notify::new());
            let release_low = Arc::new(Notify::new());
            let hold = hold_low.clone();
            let reached = reached_low.clone();
            let release = release_low.clone();
            let task = tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = server.accept().await else {
                        break;
                    };
                    let low = low.clone();
                    let videos = videos.clone();
                    let hold = hold.clone();
                    let reached = reached.clone();
                    let release = release.clone();
                    tokio::spawn(async move {
                        let mut bytes = [0_u8; 4096];
                        let size = socket.read(&mut bytes).await.unwrap_or(0);
                        let path = String::from_utf8_lossy(&bytes[..size]);
                        let data = if path.contains("/multimedia/tasks/task-1") {
                            r#"{"id":"task-1","media_type":1,"model_id":"v1","status":2,"progress":100,"result_url":"https://example.com/video.mp4"}"#.into()
                        } else if path.contains("quality_type=low") {
                            low.lock().unwrap().clone()
                        } else if path.contains("media_type=1") {
                            videos.lock().unwrap().clone()
                        } else {
                            "[]".into()
                        };
                        if path.contains("quality_type=low") && hold.swap(false, Ordering::SeqCst) {
                            reached.notify_one();
                            release.notified().await;
                        }
                        let body = format!(r#"{{"code":0,"data":{data}}}"#);
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = socket.write_all(response.as_bytes()).await;
                    });
                }
            });
            Self {
                url,
                chat_low,
                video,
                hold_low,
                reached_low,
                release_low,
                task,
            }
        }
    }

    impl Drop for MockDiscovery {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    #[tokio::test]
    async fn sync_handles_add_remove_null_partial_failure_and_account_switch() {
        let server = MockDiscovery::start().await;
        let provider = Arc::new(crate::PersonAiContextHandle::default());
        let auth = AuthorizedClientContext::new()
            .with_token("test")
            .with_product_code("test")
            .with_product_version("1.0.0");
        provider.publish(crate::AccountContext {
            account_scope: "alice".into(),
            revision: 1,
            authorization: auth.clone(),
        });
        let config = PersonAiConfig::new(&server.url);
        let services = Arc::new(PersonAiServices::new(&server.url).unwrap());
        let models = Arc::new(
            PersonAiModels::new(
                config.connection_id.clone(),
                &server.url,
                services.clone(),
                provider.clone(),
            )
            .unwrap(),
        );
        let other = ModelConnection::new(
            "other",
            Arc::new(OtherProvider),
            vec![ModelDescriptor::new("other-model")],
        );
        let runtime = ModelRuntime::new(ModelRuntimeUpdate::new("", 0, vec![other])).unwrap();
        let sync = PersonAiModelSync::new(
            config,
            services,
            provider.clone(),
            models.clone(),
            runtime.clone(),
        );

        let first = sync.refresh_now().await.unwrap();
        assert!(!first.stale);
        assert_eq!(sync.snapshot().await.unwrap().chat[0].id, "p/old");
        assert_eq!(
            sync.snapshot().await.unwrap().media[&MediaType::VIDEO][0].id,
            "v1"
        );
        assert_eq!(runtime.snapshot().chat.models().len(), 2);

        *server.chat_low.lock().unwrap() = "null".into();
        *server.video.lock().unwrap() = "null".into();
        let failed = sync.refresh_now().await.unwrap();
        assert!(failed.stale);
        assert_eq!(sync.snapshot().await.unwrap().chat[0].id, "p/old");
        assert_eq!(
            sync.snapshot().await.unwrap().media[&MediaType::VIDEO][0].id,
            "v1"
        );

        *server.chat_low.lock().unwrap() = "[]".into();
        *server.video.lock().unwrap() = "[]".into();
        let removed = sync.refresh_now().await.unwrap();
        assert!(!removed.stale);
        assert!(sync.snapshot().await.unwrap().chat.is_empty());
        assert!(sync.snapshot().await.unwrap().media[&MediaType::VIDEO].is_empty());
        assert_eq!(
            runtime.snapshot().chat.models(),
            vec![ModelRef::new("other", "other-model")]
        );

        // A task submitted before the catalog removal keeps its original adapter.
        let bound = models.bind(provider.current().unwrap());
        let old_job = VideoJob {
            model: ModelRef::new("personai", "v1"),
            protocol: "personai.multimedia".into(),
            id: "task-1".into(),
            binding_id: None,
            status: ModelJobStatus::Running,
            progress: Some(0.5),
            output: None,
            error: None,
        };
        let queried = VideoGenerationModel::query(bound.as_ref(), &old_job)
            .await
            .unwrap();
        assert_eq!(queried.status, ModelJobStatus::Completed);

        *server.chat_low.lock().unwrap() = r#"[{"model_code":"new","provider_code":"q"}]"#.into();
        provider.publish(crate::AccountContext {
            account_scope: "bob".into(),
            revision: 2,
            authorization: auth,
        });
        sync.refresh_now().await.unwrap();
        assert_eq!(sync.snapshot().await.unwrap().account_scope, "bob");
        assert!(
            runtime
                .snapshot()
                .chat
                .models()
                .contains(&ModelRef::new("personai", "q/new"))
        );
        assert!(
            runtime
                .snapshot()
                .chat
                .models()
                .contains(&ModelRef::new("other", "other-model"))
        );
    }

    #[tokio::test]
    async fn late_old_account_refresh_cannot_publish_over_new_account() {
        let server = MockDiscovery::start().await;
        let provider = Arc::new(crate::PersonAiContextHandle::default());
        let auth = AuthorizedClientContext::new()
            .with_token("test")
            .with_product_code("test")
            .with_product_version("1.0.0");
        provider.publish(crate::AccountContext {
            account_scope: "alice".into(),
            revision: 1,
            authorization: auth.clone(),
        });
        let config = PersonAiConfig::new(&server.url);
        let services = Arc::new(PersonAiServices::new(&server.url).unwrap());
        let models = Arc::new(
            PersonAiModels::new(
                config.connection_id.clone(),
                &server.url,
                services.clone(),
                provider.clone(),
            )
            .unwrap(),
        );
        let runtime = ModelRuntime::new(ModelRuntimeUpdate::new("", 0, vec![])).unwrap();
        let sync = Arc::new(PersonAiModelSync::new(
            config,
            services,
            provider.clone(),
            models,
            runtime.clone(),
        ));

        server.hold_low.store(true, Ordering::SeqCst);
        let old_refresh = {
            let sync = sync.clone();
            tokio::spawn(async move { sync.refresh_now().await })
        };
        tokio::time::timeout(Duration::from_secs(5), server.reached_low.notified())
            .await
            .unwrap();
        *server.chat_low.lock().unwrap() = r#"[{"model_code":"new","provider_code":"q"}]"#.into();
        provider.publish(crate::AccountContext {
            account_scope: "bob".into(),
            revision: 2,
            authorization: auth,
        });
        server.release_low.notify_one();
        assert!(old_refresh.await.unwrap().is_err());
        assert!(sync.snapshot().await.is_none());

        let current = sync.refresh_now().await.unwrap();
        assert_eq!(current.account_scope.as_deref(), Some("bob"));
        assert_eq!(sync.snapshot().await.unwrap().chat[0].id, "q/new");
        assert!(
            runtime
                .snapshot()
                .chat
                .models()
                .contains(&ModelRef::new("personai", "q/new"))
        );
        assert!(
            !runtime
                .snapshot()
                .chat
                .models()
                .contains(&ModelRef::new("personai", "p/old"))
        );
    }
}
