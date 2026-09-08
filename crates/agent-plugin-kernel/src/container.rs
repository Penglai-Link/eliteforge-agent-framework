use std::{
    collections::{HashMap, HashSet},
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;

use crate::{PluginError, ResourceScope, ServiceRegistry, ShutdownReport};

/// An asynchronously installed, statically linked plugin.
///
/// Installation must register cleanup immediately after acquiring a resource,
/// before the next await, so cancellation can release that resource. Detached
/// tasks require a synchronous cancellation fallback and an `on_shutdown`
/// callback for awaited graceful termination.
#[async_trait]
pub trait Plugin: Send + Sync {
    fn id(&self) -> &str;

    fn dependencies(&self) -> Vec<String> {
        Vec::new()
    }

    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError>;
}

/// Mutable registration access available only during a plugin's installation.
pub struct PluginContext<'a> {
    plugin: &'a str,
    services: &'a mut ServiceRegistry,
    cleanups: &'a mut ResourceScope,
}

impl PluginContext<'_> {
    pub fn provide<T: Send + Sync + 'static>(&mut self, value: T) -> Result<(), PluginError> {
        self.services.provide(self.plugin, value)
    }

    pub fn contribute<T: Send + Sync + 'static>(&mut self, value: T) -> Result<(), PluginError> {
        self.services.contribute(self.plugin, value)
    }

    pub fn services(&self) -> &ServiceRegistry {
        self.services
    }

    /// Register synchronous cleanup, run once in reverse registration order.
    pub fn on_cleanup(&mut self, cleanup: impl FnOnce() + Send + 'static) {
        self.cleanups.on_cleanup(cleanup);
    }
    /// Register graceful asynchronous shutdown; also register a synchronous
    /// cancellation fallback for interrupted installation or runtime teardown.
    pub fn on_shutdown<F, Fut>(&mut self, callback: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        self.cleanups.on_shutdown(callback);
    }
}

/// An isolated set of installed plugins and immutable typed services.
///
/// The container closes on drop, including when an in-progress `build` future
/// is cancelled or unwinds. Cleanup callbacks run before service references are
/// released; retained external `Arc`s can outlive the container. Cleanup panics
/// are caught so remaining callbacks still run and `Drop` never propagates a
/// callback panic. The process panic hook still runs; panic=abort cannot be
/// intercepted. Use `close_async` for awaited shutdown; Drop runs only fallbacks.
pub struct PluginContainer {
    services: ServiceRegistry,
    cleanups: Mutex<ResourceScope>,
    plugins: Vec<Arc<dyn Plugin>>,
    plugin_ids: Vec<String>,
    closed: bool,
    cleanup_panic_count: usize,
}

impl PluginContainer {
    /// Validate the entire graph, then install dependencies before dependents.
    /// Among ready plugins, original input order is the deterministic tie-break.
    /// Any failure rolls back even partially installed plugins.
    pub async fn build(plugins: Vec<Arc<dyn Plugin>>) -> Result<Self, PluginError> {
        let (ids, order) = installation_order(&plugins)?;
        // This RAII guard exists across every await in plugin installation.
        let mut container = Self {
            services: ServiceRegistry::default(),
            cleanups: Mutex::new(ResourceScope::default()),
            plugins: Vec::new(),
            plugin_ids: Vec::new(),
            closed: false,
            cleanup_panic_count: 0,
        };

        for index in order {
            let mut context = PluginContext {
                plugin: &ids[index],
                services: &mut container.services,
                cleanups: container
                    .cleanups
                    .get_mut()
                    .unwrap_or_else(|e| e.into_inner()),
            };
            if let Err(source) = plugins[index].install(&mut context).await {
                container.close_async(Duration::from_secs(5)).await;
                return Err(PluginError::InstallationFailed {
                    plugin: ids[index].clone(),
                    source: Box::new(source),
                });
            }
            container.plugins.push(plugins[index].clone());
            container.plugin_ids.push(ids[index].clone());
        }
        Ok(container)
    }

    pub fn services(&self) -> &ServiceRegistry {
        &self.services
    }

    /// Release resources once. Subsequent calls are no-ops.
    /// The registry is empty after this call; installation IDs remain available
    /// for diagnostics. Service and plugin destructors should not panic.
    pub fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        let cleanups = self.cleanups.get_mut().unwrap_or_else(|e| e.into_inner());
        self.cleanup_panic_count = cleanups.close().panics;
        self.services.clear();
        while self.plugins.pop().is_some() {}
    }

    /// Gracefully close resources in reverse registration order. Repeated calls
    /// return the accumulated report. Services stay alive until callbacks finish.
    pub async fn close_async(&mut self, timeout: Duration) -> ShutdownReport {
        let report = self
            .cleanups
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .close_async(timeout)
            .await;
        self.cleanup_panic_count = report.panics;
        self.closed = true;
        self.services.clear();
        while self.plugins.pop().is_some() {}
        report
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Successfully installed plugin IDs, in installation order.
    pub fn plugin_ids(&self) -> &[String] {
        &self.plugin_ids
    }

    /// Number of caught cleanup callback panics during explicit close.
    pub fn cleanup_panic_count(&self) -> usize {
        self.cleanup_panic_count
    }
}

impl Drop for PluginContainer {
    fn drop(&mut self) {
        self.close();
    }
}

fn installation_order(
    plugins: &[Arc<dyn Plugin>],
) -> Result<(Vec<String>, Vec<usize>), PluginError> {
    let mut ids = Vec::with_capacity(plugins.len());
    let mut positions = HashMap::new();
    for (index, plugin) in plugins.iter().enumerate() {
        let id = plugin.id().to_owned();
        if id.trim().is_empty() {
            return Err(PluginError::EmptyPluginId { index });
        }
        if positions.insert(id.clone(), index).is_some() {
            return Err(PluginError::DuplicatePluginId { id });
        }
        ids.push(id);
    }

    let mut dependencies = Vec::with_capacity(plugins.len());
    for (index, plugin) in plugins.iter().enumerate() {
        let mut required = HashSet::new();
        for dependency in plugin.dependencies() {
            let position =
                positions
                    .get(&dependency)
                    .ok_or_else(|| PluginError::MissingDependency {
                        plugin: ids[index].clone(),
                        dependency,
                    })?;
            required.insert(*position);
        }
        dependencies.push(required);
    }

    let mut installed = HashSet::new();
    let mut order = Vec::with_capacity(plugins.len());
    while order.len() < plugins.len() {
        let ready = (0..plugins.len())
            .find(|index| !installed.contains(index) && dependencies[*index].is_subset(&installed));
        let Some(index) = ready else {
            return Err(PluginError::DependencyCycle {
                plugins: ids
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| !installed.contains(index))
                    .map(|(_, id)| id.clone())
                    .collect(),
            });
        };
        installed.insert(index);
        order.push(index);
    }
    Ok((ids, order))
}
