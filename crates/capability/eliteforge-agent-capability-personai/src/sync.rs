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
                let _ = sync.refresh_now().await;
            }
        })
    }

    /// Refresh domains independently. A missing or null `data` field is an error;
    /// a genuine `data: []` removes that domain's previous entries.
    pub async fn refresh_now(&self) -> Result<PersonAiSyncStatus, String> {
        let _guard = self.lock.lock().await;
        let Some(account) = self.context.current() else {
            self.runtime
                .replace(eliteforge_agent_framework::ModelRuntimeUpdate::new(
                    "",
                    0,
                    vec![],
                ))
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
        if previous.is_none() {
            self.runtime
                .replace(eliteforge_agent_framework::ModelRuntimeUpdate::new(
                    &account.account_scope,
                    account.revision,
                    vec![],
                ))
                .map_err(|error| error.to_string())?;
            if let Some(store) = self.store.read().await.clone() {
                if let Ok(Some(cached)) = store.load(&account.account_scope).await {
                    if let Some(cached_snapshot) = Self::restore_snapshot(&cached) {
                        if self.is_current(&account) {
                            self.publish(&account, &cached_snapshot).await?;
                            restored = Some(cached_snapshot);
                        }
                    }
                }
            }
        }
        let mut snapshot = previous
            .as_deref()
            .cloned()
            .or(restored)
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
                mark_error(&mut status, "cache", error.to_string());
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
        self.runtime
            .replace(update)
            .map_err(|error| error.to_string())
    }

    fn persist_snapshot(&self, snapshot: &PersonAiDiscoverySnapshot) -> ModelCatalogSnapshot {
        self.models.catalog_snapshot(snapshot, &self.config)
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
}
