use std::sync::Arc;

use crate::Plugin;

/// A reusable configuration that expands into ordinary plugins.
///
/// Expansion is synchronous and must not acquire resources or install services.
/// All members enter the same dependency graph as individually registered plugins,
/// retaining their IDs, dependency checks, rollback and shutdown callbacks.
pub trait PluginPack {
    fn into_plugins(self) -> Vec<Arc<dyn Plugin>>;
}

/// Compose plugins and nested packs when implementing an application-specific pack.
#[derive(Clone, Default)]
pub struct PluginPackBuilder {
    plugins: Vec<Arc<dyn Plugin>>,
}

impl PluginPackBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn plugin(self, plugin: impl Plugin + 'static) -> Self {
        self.plugin_arc(Arc::new(plugin))
    }

    pub fn plugin_arc(mut self, plugin: Arc<dyn Plugin>) -> Self {
        self.plugins.push(plugin);
        self
    }

    /// Append a pack in place. Nested packs do not create separate containers.
    pub fn plugin_pack(mut self, pack: impl PluginPack) -> Self {
        self.plugins.extend(pack.into_plugins());
        self
    }
}

impl PluginPack for PluginPackBuilder {
    fn into_plugins(self) -> Vec<Arc<dyn Plugin>> {
        self.plugins
    }
}
