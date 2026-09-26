use crate::{Plugin, PluginContext, PluginError, PluginReadyContext};
use async_trait::async_trait;
use std::sync::{Arc, Mutex};

type Factory = dyn Fn() -> Result<Arc<dyn Plugin>, PluginError> + Send + Sync;

/// Rebuildable plugin declaration. Dependencies are checked before the factory runs.
#[derive(Clone)]
pub struct ConfiguredPluginFactory {
    id: String,
    dependencies: Vec<String>,
    factory: Arc<Factory>,
}
impl ConfiguredPluginFactory {
    pub fn new(
        id: impl Into<String>,
        dependencies: Vec<String>,
        factory: impl Fn() -> Result<Arc<dyn Plugin>, PluginError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            dependencies,
            factory: Arc::new(factory),
        }
    }
    pub fn from_config<P: Plugin + Clone + 'static>(configuration: P) -> Self {
        let id = configuration.id().to_owned();
        let dependencies = configuration.dependencies();
        Self::new(id, dependencies, move || {
            Ok(Arc::new(configuration.clone()))
        })
    }
    pub fn plugin(&self) -> Arc<dyn Plugin> {
        Arc::new(DeferredPlugin {
            declaration: self.clone(),
            instance: Mutex::new(None),
        })
    }
}
struct DeferredPlugin {
    declaration: ConfiguredPluginFactory,
    instance: Mutex<Option<Arc<dyn Plugin>>>,
}
#[async_trait]
impl Plugin for DeferredPlugin {
    fn id(&self) -> &str {
        &self.declaration.id
    }
    fn dependencies(&self) -> Vec<String> {
        self.declaration.dependencies.clone()
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let instance = (self.declaration.factory)()?;
        if instance.id() != self.id() || instance.dependencies() != self.dependencies() {
            return Err(PluginError::message(
                "factory identity or dependencies differ from declaration",
            ));
        }
        *self.instance.lock().unwrap_or_else(|e| e.into_inner()) = Some(instance.clone());
        instance.install(context).await
    }
    async fn ready(&self, context: &mut PluginReadyContext<'_>) -> Result<(), PluginError> {
        let instance = self
            .instance
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| PluginError::message("factory has not installed"))?;
        instance.ready(context).await
    }
}
