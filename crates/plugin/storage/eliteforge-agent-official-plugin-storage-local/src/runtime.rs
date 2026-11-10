//! Host-owned storage shared by replaceable Framework assemblies.
use super::*;
use eliteforge_agent_plugin_core::{PluginContainer, ServiceRegistry, ShutdownReport};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tokio::sync::{RwLock, watch};

/// Owns the complete local storage plugin independently of any consuming Framework.
/// Close all borrowing Frameworks before awaiting [`Self::close`].
#[derive(Clone)]
pub struct LocalStorageRuntime(Arc<OwnedStorage>);

struct OwnedStorage {
    services: ServiceRegistry,
    container: tokio::sync::Mutex<PluginContainer>,
    borrowers: Arc<RwLock<()>>,
    closing: AtomicBool,
    completion: Mutex<Option<watch::Receiver<Option<ShutdownReport>>>>,
}

impl LocalStorageRuntime {
    /// Install storage once. Existing `LocalStorageConfig::database` ownership
    /// remains unchanged: an injected database is still owned by its host.
    pub async fn open(config: LocalStorageConfig) -> Result<Self, PluginError> {
        let container =
            PluginContainer::build(vec![Arc::new(LocalStoragePlugin::with_config(config))]).await?;
        Ok(Self(Arc::new(OwnedStorage {
            services: container.services().clone(),
            container: tokio::sync::Mutex::new(container),
            borrowers: Arc::new(RwLock::new(())),
            closing: AtomicBool::new(false),
            completion: Mutex::new(None),
        })))
    }

    /// Register the same services and extensions without transferring cleanup ownership.
    pub fn plugin(&self) -> BorrowedLocalStoragePlugin {
        BorrowedLocalStoragePlugin(self.clone())
    }

    /// Application services can use the same registry as borrowing Frameworks.
    pub fn services(&self) -> Result<&ServiceRegistry, PluginError> {
        if self.0.closing.load(Ordering::Acquire) {
            return Err(PluginError::message("local storage runtime is closing"));
        }
        Ok(&self.0.services)
    }

    /// Stop new borrowers, drain existing ones, then close storage exactly once.
    /// Cancelling a caller does not cancel the owned shutdown task.
    pub async fn close(&self) -> ShutdownReport {
        let mut completion = {
            let mut state = self.0.completion.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(receiver) = &*state {
                receiver.clone()
            } else {
                self.0.closing.store(true, Ordering::Release);
                let (sender, receiver) = watch::channel(None);
                *state = Some(receiver.clone());
                let owner = self.0.clone();
                tokio::spawn(async move {
                    let result = tokio::spawn(async move {
                        let _exclusive = owner.borrowers.write().await;
                        owner
                            .container
                            .lock()
                            .await
                            .close_async(Duration::from_secs(5))
                            .await
                    })
                    .await;
                    sender.send_replace(Some(result.unwrap_or_else(|error| ShutdownReport {
                        errors: vec![format!("local storage shutdown owner failed: {error}")],
                        panics: usize::from(error.is_panic()),
                        timeouts: 0,
                    })));
                });
                receiver
            }
        };
        loop {
            if let Some(report) = completion.borrow_and_update().clone() {
                return report;
            }
            if completion.changed().await.is_err() {
                return ShutdownReport {
                    errors: vec!["local storage shutdown owner terminated".into()],
                    panics: 0,
                    timeouts: 0,
                };
            }
        }
    }
}

/// A non-owning `storage.local` registration. Dropping or closing its container
/// releases only its borrow lease, including failed/cancelled installations.
#[derive(Clone)]
pub struct BorrowedLocalStoragePlugin(LocalStorageRuntime);

#[async_trait]
impl Plugin for BorrowedLocalStoragePlugin {
    fn id(&self) -> &str {
        "storage.local"
    }

    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let lease = self.0.0.borrowers.clone().read_owned().await;
        let services = self.0.services()?;
        context.on_cleanup(move || drop(lease));
        macro_rules! service {
            ($kind:ident) => {
                services.get::<$kind>().ok_or_else(|| {
                    PluginError::message(concat!(
                        "missing local storage service: ",
                        stringify!($kind)
                    ))
                })?
            };
        }
        macro_rules! provide {
            ($kind:ident) => {
                context.provide($kind(service!($kind).0.clone()))?
            };
        }
        context.provide(service!(SqliteDatabase).as_ref().clone())?;
        provide!(TransactionManagerService);
        provide!(SessionLifecycleStoreService);
        provide!(StateStoreService);
        provide!(InteractionStoreService);
        provide!(SessionStoreService);
        provide!(HistoryStoreService);
        provide!(ConversationStoreService);
        provide!(LongTermMemoryService);
        provide!(SummaryStoreService);
        provide!(ArtifactStoreService);
        provide!(ModelCatalogStoreService);
        provide!(SqliteCapabilitiesService);
        context.contribute_extension(service!(TransactionManagerService).0.clone())?;
        context.contribute_extension(service!(SessionLifecycleStoreService).0.clone())?;
        context.contribute_extension(service!(StateStoreService).0.clone())?;
        context.contribute_extension(service!(InteractionStoreService).0.clone())?;
        context.contribute_extension(service!(SessionStoreService).0.clone())?;
        context.contribute_extension(service!(ConversationStoreService).0.clone())?;
        context.contribute_extension(service!(LongTermMemoryService).0.clone())?;
        context.contribute_extension(service!(SummaryStoreService).0.clone())?;
        context.contribute_extension(service!(ArtifactStoreService).0.clone())?;
        context.contribute_extension(service!(ModelCatalogStoreService).0.clone())?;
        Ok(())
    }
}
