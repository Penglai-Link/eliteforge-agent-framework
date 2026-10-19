//! Per-account discovery with independent domain updates.
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crab_serverapi::{
    base::AuthorizedClientApi,
    gateway::{ChatModelInfo, ProtocolType, QualityType},
    multimedia::{MediaType, MultimediaModelInfo},
};
use eliteforge_agent_core::{ModelCatalogSnapshot, ModelCatalogStore};
use eliteforge_agent_framework::{ModelAccountIdentity, ModelCatalogRefresh};
use eliteforge_agent_framework::{ModelRuntime, ModelRuntimeUpdate};
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
    #[serde(default)]
    pub dimension: Option<usize>,
    #[serde(default)]
    pub tags: Vec<String>,
}

impl From<MultimediaModelInfo> for DiscoveredMediaModel {
    fn from(value: MultimediaModelInfo) -> Self {
        Self {
            id: value.model_id,
            model_code: value.model_code,
            model_name: value.model_name,
            media_type: value.media_type,
            dimension: value.dimension,
            tags: value.tags,
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

struct RefreshCompletion {
    account: Option<Arc<AccountContext>>,
    result: Result<PersonAiSyncStatus, String>,
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
    lock: Mutex<Option<RefreshCompletion>>,
    completed_refreshes: AtomicU64,
    binding_generation: AtomicU64,
    refresh: Arc<Notify>,
    changes: watch::Sender<Option<Arc<PersonAiDiscoverySnapshot>>>,
    bindings: watch::Sender<ModelRuntimeUpdate>,
    closed: CancellationToken,
    subscribers: std::sync::Mutex<Vec<std::sync::Weak<ModelRuntime>>>,
}

#[async_trait::async_trait]
impl ModelCatalogRefresh for PersonAiModelSync {
    fn request_refresh(&self) {
        PersonAiModelSync::request_refresh(self);
    }

    fn connection_id(&self) -> &str {
        &self.config.connection_id
    }

    fn account_identity(&self) -> Option<ModelAccountIdentity> {
        self.context.current().map(|account| ModelAccountIdentity {
            scope: account.account_scope.clone(),
            revision: account.revision,
        })
    }

    async fn refresh(&self) -> Result<ModelRuntimeUpdate, eliteforge_agent_core::ModelError> {
        let account = self.context.current();
        let result = self.refresh_now().await;
        if account
            .as_ref()
            .is_none_or(|account| !self.is_current(account))
        {
            return Err(eliteforge_agent_core::ModelError::Provider(Box::new(
                eliteforge_agent_core::ProviderModelError {
                    authorization_attempt: None,
                    provider: "personai".into(),
                    category: eliteforge_agent_core::ProviderErrorCategory::AccountChanged,
                    http_status: None,
                    business_code: None,
                    error_type: None,
                    model: None,
                    message: "PersonAI account changed during catalog refresh".into(),
                },
            )));
        }
        result.map_err(eliteforge_agent_core::ModelError::Transient)?;
        Ok(self.bindings.borrow().clone())
    }
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
        let (bindings, _) = watch::channel(ModelRuntimeUpdate::new("", 0, vec![]));
        let refresh = models.refresh.clone();
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
            lock: Mutex::new(None),
            completed_refreshes: AtomicU64::new(0),
            binding_generation: AtomicU64::new(0),
            refresh,
            changes,
            bindings,
            closed: CancellationToken::new(),
            subscribers: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<Option<Arc<PersonAiDiscoverySnapshot>>> {
        self.changes.subscribe()
    }

    pub(crate) fn attach_runtime(&self, runtime: &Arc<ModelRuntime>) -> Result<(), String> {
        let mut subscribers = self
            .subscribers
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        runtime
            .replace_connection(&self.config.connection_id, self.bindings.borrow().clone())
            .map_err(|error| error.to_string())?;
        subscribers.push(Arc::downgrade(runtime));
        Ok(())
    }

    pub(crate) fn detach_runtime(&self, runtime: &Arc<ModelRuntime>) {
        self.subscribers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retain(|subscriber| {
                subscriber
                    .upgrade()
                    .is_some_and(|current| !Arc::ptr_eq(&current, runtime))
            });
    }

    fn publish_bindings(&self, update: ModelRuntimeUpdate) -> Result<(), String> {
        // Registration and publication share this lock: joining Agents cannot
        // miss a revision, and completed refreshes are immediately observable.
        let mut subscribers = self
            .subscribers
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        subscribers.retain(|subscriber| subscriber.strong_count() > 0);
        let mut errors = Vec::new();
        for runtime in subscribers.iter().filter_map(std::sync::Weak::upgrade) {
            if let Err(error) =
                runtime.replace_connection(&self.config.connection_id, update.clone())
            {
                errors.push(error.to_string());
            }
        }
        // One Agent's invalid local configuration cannot withhold a connection
        // revision from other Agents or from subscribers that attach later.
        self.bindings.send_replace(update);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    pub(crate) fn context_updates(&self) -> watch::Receiver<Option<Arc<AccountContext>>> {
        self.context.subscribe()
    }

    fn empty_update(&self, scope: &str, revision: u64) -> ModelRuntimeUpdate {
        let mut update = ModelRuntimeUpdate::new(scope, revision, vec![]);
        update.generation = self.binding_generation.fetch_add(1, Ordering::AcqRel) + 1;
        update
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
        if !self.closed.is_cancelled() {
            self.refresh.notify_one();
        }
    }

    pub(crate) fn shutdown(&self) {
        self.closed.cancel();
    }

    pub(crate) async fn close(&self) -> Result<(), String> {
        self.shutdown();
        let _guard = self.lock.lock().await;
        self.invalidate().await
    }

    pub(crate) fn start(
        self: &Arc<Self>,
        stop: CancellationToken,
        mut context_updates: watch::Receiver<Option<Arc<AccountContext>>>,
    ) -> tokio::task::JoinHandle<()> {
        let sync = self.clone();
        tokio::spawn(async move {
            let interval = sync.config.sync_interval.max(Duration::from_secs(1));
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // The host connection has already completed the initial network refresh.
            ticker.tick().await;
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
                let refresh = sync.refresh_now();
                tokio::pin!(refresh);
                loop {
                    tokio::select! {
                        biased;
                        _ = stop.cancelled() => return,
                        // Notifications arriving during this round share it;
                        // do not leave a permit that starts a duplicate round.
                        _ = sync.refresh.notified() => {},
                        _ = &mut refresh => break,
                    }
                }
            }
        })
    }

    /// Refresh domains independently. A missing or null `data` field is an error;
    /// a genuine `data: []` removes that domain's previous entries.
    pub async fn refresh_now(&self) -> Result<PersonAiSyncStatus, String> {
        let observed = self.completed_refreshes.load(Ordering::Acquire);
        let mut context_updates = self.context.subscribe();
        let mut guard = tokio::select! {
            biased;
            _ = self.closed.cancelled() => return Err("PersonAI connection is closed".into()),
            guard = self.lock.lock() => guard,
        };
        context_updates.borrow_and_update();
        let account = self.context.current();
        if self.completed_refreshes.load(Ordering::Acquire) != observed
            && let Some(completed) = &*guard
            && match (&completed.account, &account) {
                (Some(previous), Some(current)) => Arc::ptr_eq(previous, current),
                (None, None) => true,
                _ => false,
            }
        {
            return completed.result.clone();
        }
        let result = tokio::select! {
            biased;
            _ = self.closed.cancelled() => Err("PersonAI connection is closed".into()),
            changed = context_updates.changed() => {
                let _ = changed;
                self.invalidate().await?;
                Err("PersonAI account context changed during synchronization".into())
            }
            result = self.refresh_once() => result,
        };
        *guard = Some(RefreshCompletion {
            account,
            result: result.clone(),
        });
        self.completed_refreshes.fetch_add(1, Ordering::Release);
        result
    }

    async fn invalidate(&self) -> Result<(), String> {
        let account = self.context.current();
        let update = self.empty_update(
            account
                .as_ref()
                .map_or("", |account| account.account_scope.as_str()),
            account.as_ref().map_or(0, |account| account.revision),
        );
        self.runtime
            .replace_connection(&self.config.connection_id, update.clone())
            .map_err(|error| error.to_string())?;
        self.publish_bindings(update)?;
        let mut state = self.state.write().await;
        state.snapshot = None;
        state.status = PersonAiSyncStatus::default();
        self.changes.send_replace(None);
        Ok(())
    }

    async fn refresh_once(&self) -> Result<PersonAiSyncStatus, String> {
        let Some(account) = self.context.current() else {
            let update = self.empty_update("", 0);
            self.runtime
                .replace_connection(&self.config.connection_id, update.clone())
                .map_err(|error| error.to_string())?;
            let mut state = self.state.write().await;
            state.snapshot = None;
            state.status = PersonAiSyncStatus::default();
            self.changes.send_replace(None);
            self.publish_bindings(update)?;
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
            let update = self.empty_update(&account.account_scope, account.revision);
            self.runtime
                .replace_connection(&self.config.connection_id, update.clone())
                .map_err(|error| error.to_string())?;
            self.publish_bindings(update)?;
            if let Some(previous) = &previous {
                let mut current = (**previous).clone();
                current.account_revision = account.revision;
                if self.is_current(&account) {
                    self.publish(&account, &current).await?;
                    restored = Some(current);
                }
            } else if let Some(store) = self.store.read().await.clone()
                && let Ok(Some(cached)) = store.load(&self.cache_key(&account.account_scope)).await
                && let Some(mut cached_snapshot) = Self::restore_snapshot(&cached)
            {
                cached_snapshot.account_scope = account.account_scope.clone();
                cached_snapshot.account_revision = account.revision;
                if self.is_current(&account) {
                    restored = Some(cached_snapshot);
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
            MediaType::EMBEDDING,
            crate::models::PERSONAI_RERANK_MEDIA_TYPE,
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
            .iter()
            .any(|(name, domain)| name != "cache" && domain.last_error.is_some());
        if previous.is_none() && status.stale {
            // A new account cannot become usable from a cache or a partially
            // successful initial refresh. Existing accounts retain old domains.
            self.state.write().await.status = status.clone();
            return Ok(status);
        }
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
        let mut update = self
            .models
            .runtime_update(account.clone(), snapshot, &self.config)
            .map_err(|error| error.to_string())?;
        update.generation = self.binding_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let replaced = self
            .runtime
            .replace_connection_if_current(
                &self.config.connection_id,
                &account.account_scope,
                account.revision,
                update.clone(),
            )
            .map_err(|error| error.to_string())?;
        if replaced {
            self.publish_bindings(update)?;
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
        // JSON tuple encoding avoids delimiter collisions between host identifiers.
        format!(
            "personai:{}",
            serde_json::json!([
                self.config.server_base_url.trim_end_matches('/'),
                self.config.connection_id,
                account_scope,
            ])
        )
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
pub(crate) mod tests {
    use super::*;
    use crab_serverapi::base::AuthorizedClientContext;
    use eliteforge_agent_core::{
        ChatModel, ChatRequest, ChatResponse, ChatStream, ModelConnection, ModelDescriptor,
        ModelError, ModelJobStatus, ModelRef, VideoGenerationModel, VideoJob,
    };
    use eliteforge_agent_framework::{ModelRuntime, ModelRuntimeUpdate};
    use std::sync::{
        Mutex as StdMutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
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

    pub(crate) struct MockDiscovery {
        pub(crate) url: String,
        pub(crate) requests: Arc<AtomicUsize>,
        pub(crate) chat_low: Arc<StdMutex<String>>,
        pub(crate) video: Arc<StdMutex<String>>,
        hold_low: Arc<AtomicBool>,
        reached_low: Arc<Notify>,
        release_low: Arc<Notify>,
        task: tokio::task::JoinHandle<()>,
    }

    struct OtherProvider;

    #[tokio::test]
    async fn invalid_subscriber_does_not_block_healthy_or_later_agents() {
        let config = PersonAiConfig::new("http://127.0.0.1:1");
        let context = Arc::new(crate::PersonAiContextHandle::default());
        let services = Arc::new(PersonAiServices::new(&config.server_base_url).unwrap());
        let models = Arc::new(
            PersonAiModels::new(
                config.connection_id.clone(),
                &config.server_base_url,
                services.clone(),
                context.clone(),
            )
            .unwrap(),
        );
        let empty = || ModelRuntime::new(ModelRuntimeUpdate::new("", 0, vec![])).unwrap();
        let sync = PersonAiModelSync::new(config, services, context, models, empty());
        let binding = |model| {
            ModelConnection::new(
                "personai",
                Arc::new(OtherProvider),
                vec![ModelDescriptor::new(model)],
            )
        };
        // This Agent already owns the provider connection ID in a static
        // partition, so its next provider update will violate uniqueness.
        let invalid = ModelRuntime::new(ModelRuntimeUpdate::new(
            "",
            0,
            vec![binding("static-model")],
        ))
        .unwrap();
        let healthy = empty();
        sync.attach_runtime(&invalid).unwrap();
        sync.attach_runtime(&healthy).unwrap();
        assert!(
            sync.publish_bindings(ModelRuntimeUpdate::new(
                "alice",
                1,
                vec![binding("fresh-model")]
            ))
            .is_err()
        );
        assert_eq!(
            healthy.snapshot().chat.models(),
            vec![ModelRef::new("personai", "fresh-model")]
        );
        assert_eq!(
            invalid.snapshot().chat.models(),
            vec![ModelRef::new("personai", "static-model")]
        );
        let later = empty();
        sync.attach_runtime(&later).unwrap();
        assert_eq!(
            later.snapshot().chat.models(),
            healthy.snapshot().chat.models()
        );
    }

    #[derive(Default)]
    struct TestCatalogStore {
        saved: StdMutex<BTreeMap<String, ModelCatalogSnapshot>>,
        loads: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl ModelCatalogStore for TestCatalogStore {
        async fn load(&self, key: &str) -> Result<Option<ModelCatalogSnapshot>, ModelError> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            Ok(self.saved.lock().unwrap().get(key).cloned())
        }
        async fn save(&self, value: &ModelCatalogSnapshot) -> Result<(), ModelError> {
            self.saved
                .lock()
                .unwrap()
                .insert(value.account_scope.clone(), value.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn cached_catalog_never_bypasses_network_initialization_and_is_server_scoped() {
        let server = MockDiscovery::start().await;
        let other_server = MockDiscovery::start().await;
        let provider = Arc::new(crate::PersonAiContextHandle::default());
        provider.publish(crate::AccountContext {
            account_scope: "alice".into(),
            revision: 1,
            authorization: AuthorizedClientContext::new()
                .with_token("secret-not-for-model-cache")
                .with_product_code("test")
                .with_product_version("1.0.0"),
        });
        let store = Arc::new(TestCatalogStore::default());
        let connection = crate::PersonAiConnection::connect(
            PersonAiConfig::new(&server.url),
            provider.clone(),
            Some(store.clone()),
        )
        .await
        .unwrap();
        connection.close().await.unwrap();
        let requests_before = server.requests.load(Ordering::SeqCst);
        let loads_before = store.loads.load(Ordering::SeqCst);
        *server.chat_low.lock().unwrap() = "null".into();
        assert!(
            crate::PersonAiConnection::connect(
                PersonAiConfig::new(&server.url),
                provider.clone(),
                Some(store.clone()),
            )
            .await
            .is_err()
        );
        assert!(server.requests.load(Ordering::SeqCst) > requests_before);
        assert!(store.loads.load(Ordering::SeqCst) > loads_before);
        let other = crate::PersonAiConnection::connect(
            PersonAiConfig::new(&other_server.url),
            provider,
            Some(store.clone()),
        )
        .await
        .unwrap();
        other.close().await.unwrap();
        let saved = store.saved.lock().unwrap();
        assert_eq!(saved.len(), 2);
        assert!(
            !serde_json::to_string(&*saved)
                .unwrap()
                .contains("secret-not-for-model-cache")
        );
    }

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
        pub(crate) async fn start() -> Self {
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
            let requests = Arc::new(AtomicUsize::new(0));
            let count = requests.clone();
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
                    let count = count.clone();
                    tokio::spawn(async move {
                        let mut bytes = [0_u8; 4096];
                        let size = socket.read(&mut bytes).await.unwrap_or(0);
                        let path = String::from_utf8_lossy(&bytes[..size]);
                        count.fetch_add(1, Ordering::SeqCst);
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
                requests,
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
    async fn connection_close_interrupts_manual_refresh_without_waiting_for_server() {
        let server = MockDiscovery::start().await;
        let provider = Arc::new(crate::PersonAiContextHandle::default());
        provider.publish(crate::AccountContext {
            account_scope: "alice".into(),
            revision: 1,
            authorization: AuthorizedClientContext::new()
                .with_token("test")
                .with_product_code("test")
                .with_product_version("1.0.0"),
        });
        let connection =
            crate::PersonAiConnection::connect(PersonAiConfig::new(&server.url), provider, None)
                .await
                .unwrap();
        server.hold_low.store(true, Ordering::SeqCst);
        let sync = connection.sync();
        let refresh = tokio::spawn({
            let sync = sync.clone();
            async move { sync.refresh_now().await }
        });
        tokio::time::timeout(Duration::from_secs(2), server.reached_low.notified())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), connection.close())
            .await
            .unwrap()
            .unwrap();
        assert!(refresh.await.unwrap().unwrap_err().contains("closed"));
        assert!(sync.refresh_now().await.unwrap_err().contains("closed"));
        assert!(sync.snapshot().await.is_none());
        server.release_low.notify_one();
    }

    #[tokio::test]
    async fn account_switch_interrupts_blocked_background_discovery() {
        let server = MockDiscovery::start().await;
        let provider = Arc::new(crate::PersonAiContextHandle::default());
        let authorization = AuthorizedClientContext::new()
            .with_token("test")
            .with_product_code("test")
            .with_product_version("1.0.0");
        provider.publish(crate::AccountContext {
            account_scope: "alice".into(),
            revision: 1,
            authorization: authorization.clone(),
        });
        let connection = crate::PersonAiConnection::connect(
            PersonAiConfig::new(&server.url),
            provider.clone(),
            None,
        )
        .await
        .unwrap();
        let old_binding = connection.models.bind_current().unwrap();
        let sync = connection.sync();
        let mut changes = sync.subscribe();
        server.hold_low.store(true, Ordering::SeqCst);
        sync.request_refresh();
        tokio::time::timeout(Duration::from_secs(2), server.reached_low.notified())
            .await
            .unwrap();
        provider.publish(crate::AccountContext {
            account_scope: "bob".into(),
            revision: 2,
            authorization,
        });
        assert!(old_binding.ensure_current().is_err());
        // Bob must become usable before Alice's server request is released.
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if changes
                    .borrow_and_update()
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.account_scope == "bob")
                {
                    break;
                }
                changes.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        server.release_low.notify_one();
        assert_eq!(sync.snapshot().await.unwrap().account_scope, "bob");
        connection.close().await.unwrap();
    }

    #[tokio::test]
    async fn background_refresh_notifications_join_the_active_round() {
        let server = MockDiscovery::start().await;
        let provider = Arc::new(crate::PersonAiContextHandle::default());
        provider.publish(crate::AccountContext {
            account_scope: "alice".into(),
            revision: 1,
            authorization: AuthorizedClientContext::new()
                .with_token("test")
                .with_product_code("test")
                .with_product_version("1.0.0"),
        });
        let connection =
            crate::PersonAiConnection::connect(PersonAiConfig::new(&server.url), provider, None)
                .await
                .unwrap();
        let sync = connection.sync();
        let mut changes = sync.subscribe();
        changes.borrow_and_update();
        server.hold_low.store(true, Ordering::SeqCst);
        sync.request_refresh();
        tokio::time::timeout(Duration::from_secs(2), server.reached_low.notified())
            .await
            .unwrap();
        for _ in 0..10 {
            sync.request_refresh();
        }
        server.release_low.notify_one();
        tokio::time::timeout(Duration::from_secs(2), changes.changed())
            .await
            .unwrap()
            .unwrap();
        // Every response in the joined round has arrived; a queued duplicate
        // would publish another snapshot immediately, not at the 5 minute tick.
        assert!(
            tokio::time::timeout(Duration::from_millis(100), changes.changed())
                .await
                .is_err()
        );
        // Initial discovery plus one joined refresh: three chat tiers and
        // seven media domains (including embedding and native rerank).
        assert_eq!(server.requests.load(Ordering::SeqCst), 20);
        connection.close().await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_refreshes_share_one_network_round() {
        let server = MockDiscovery::start().await;
        let provider = Arc::new(crate::PersonAiContextHandle::default());
        provider.publish(crate::AccountContext {
            account_scope: "alice".into(),
            revision: 1,
            authorization: AuthorizedClientContext::new()
                .with_token("test")
                .with_product_code("test")
                .with_product_version("1.0.0"),
        });
        let connection =
            crate::PersonAiConnection::connect(PersonAiConfig::new(&server.url), provider, None)
                .await
                .unwrap();
        let sync = connection.sync();
        server.hold_low.store(true, Ordering::SeqCst);
        let (results, ()) = tokio::join!(
            async { tokio::join!(sync.refresh_now(), sync.refresh_now(), sync.refresh_now()) },
            async {
                server.reached_low.notified().await;
                server.release_low.notify_one();
            },
        );
        assert!(results.0.is_ok() && results.1.is_ok() && results.2.is_ok());
        assert_eq!(server.requests.load(Ordering::SeqCst), 20);
        connection.close().await.unwrap();
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

        let in_flight = runtime.snapshot();
        let selected = ModelRef::new("personai", "p/old");
        *server.chat_low.lock().unwrap() = r#"[{"model_code":"old","provider_code":"p","protocol_types":["responses"],"context_length":16384}]"#.into();
        sync.refresh_now().await.unwrap();
        assert_eq!(
            runtime.snapshot().chat.protocol(&selected),
            Some("personai.responses")
        );
        assert_eq!(
            runtime
                .snapshot()
                .chat
                .get(&selected)
                .unwrap()
                .context_window_tokens,
            Some(16384)
        );
        assert_eq!(
            in_flight.chat.protocol(&selected),
            Some("personai.chat-completions")
        );
        assert_eq!(
            in_flight.chat.get(&selected).unwrap().context_window_tokens,
            Some(8192)
        );

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
