use std::sync::Arc;

use eliteforge_agent_core::ModelCatalogStore;
use eliteforge_agent_framework::{ModelRuntime, ModelRuntimeUpdate};
use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;

use crate::{
    AccountContext, PersonAiConfig, PersonAiContextProvider, PersonAiModelSync, PersonAiModels,
    PersonAiServices,
};

/// Host-owned, authenticated PersonAI connection shared by any number of Agents.
/// Construction performs discovery over the network before returning a usable connection.
pub struct PersonAiConnection {
    pub(crate) config: PersonAiConfig,
    pub(crate) services: Arc<PersonAiServices>,
    pub(crate) models: Arc<PersonAiModels>,
    pub(crate) sync: Arc<PersonAiModelSync>,
    stop: CancellationToken,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

struct ConnectionContext {
    source: Arc<dyn PersonAiContextProvider>,
    stop: CancellationToken,
}

impl PersonAiContextProvider for ConnectionContext {
    fn is_closed(&self) -> bool {
        self.stop.is_cancelled()
    }

    fn current(&self) -> Option<Arc<AccountContext>> {
        if self.stop.is_cancelled() {
            None
        } else {
            self.source.current()
        }
    }

    fn subscribe(&self) -> watch::Receiver<Option<Arc<AccountContext>>> {
        self.source.subscribe()
    }
}

impl PersonAiConnection {
    pub async fn connect(
        config: PersonAiConfig,
        context: Arc<dyn PersonAiContextProvider>,
        store: Option<Arc<dyn ModelCatalogStore>>,
    ) -> Result<Arc<Self>, String> {
        if context.current().is_none() {
            return Err("PersonAI connection requires an authenticated account context".into());
        }
        let stop = CancellationToken::new();
        let context: Arc<dyn PersonAiContextProvider> = Arc::new(ConnectionContext {
            source: context,
            stop: stop.clone(),
        });
        let services = Arc::new(
            PersonAiServices::new(&config.server_base_url).map_err(|error| error.to_string())?,
        );
        let models = Arc::new(
            PersonAiModels::new(
                config.connection_id.clone(),
                &config.server_base_url,
                services.clone(),
                context.clone(),
            )
            .map_err(|error| error.to_string())?,
        );
        // This runtime contains only this connection, never an Agent's base models or routing.
        let runtime = ModelRuntime::new(ModelRuntimeUpdate::new("", 0, vec![]))
            .map_err(|error| error.to_string())?;
        let sync = Arc::new(PersonAiModelSync::new(
            config.clone(),
            services.clone(),
            context,
            models.clone(),
            runtime,
        ));
        sync.set_store(store).await;
        // Subscribe before discovery so a switch between its completion and
        // background task startup cannot be lost.
        let context_updates = sync.context_updates();
        let status = sync.refresh_now().await?;
        let failed: Vec<_> = status
            .domains
            .iter()
            .filter(|(domain, status)| domain.as_str() != "cache" && status.last_error.is_some())
            .map(|(domain, _)| domain.as_str())
            .collect();
        if status.account_scope.is_none() || !failed.is_empty() {
            return Err(format!(
                "PersonAI initial discovery failed for domains: {}",
                failed.join(", ")
            ));
        }
        let task = sync.start(stop.clone(), context_updates);
        Ok(Arc::new(Self {
            config,
            services,
            models,
            sync,
            stop,
            task: Mutex::new(Some(task)),
        }))
    }

    pub fn services(&self) -> Arc<PersonAiServices> {
        self.services.clone()
    }

    pub fn sync(&self) -> Arc<PersonAiModelSync> {
        self.sync.clone()
    }

    pub fn is_closed(&self) -> bool {
        self.stop.is_cancelled()
    }

    /// Stops shared discovery and invalidates new model calls on every attached Agent.
    /// Closing an individual Agent does not call this method.
    pub async fn close(&self) -> Result<(), String> {
        self.stop.cancel();
        self.sync.close().await?;
        let mut task = self.task.lock().await;
        if let Some(handle) = task.take() {
            handle.await.map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

impl Drop for PersonAiConnection {
    fn drop(&mut self) {
        self.stop.cancel();
        self.sync.shutdown();
    }
}
