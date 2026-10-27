use crate::*;
use eliteforge_agent_plugin_core::{Plugin, PluginContainer};
use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

struct BoundCapabilityPack {
    original: Arc<dyn CapabilityPack>,
    included: Vec<Arc<dyn CapabilityPack>>,
}
impl CapabilityPack for BoundCapabilityPack {
    fn id(&self) -> &str {
        self.original.id()
    }
    fn configuration(&self) -> &CapabilityConfig {
        self.original.configuration()
    }
    fn preflight(&self) -> Result<(), AgentError> {
        self.original.preflight()
    }
    fn requires_skills_runtime(&self) -> bool {
        self.original.requires_skills_runtime()
    }
    fn plugins(&self) -> Vec<Arc<dyn Plugin>> {
        self.original.plugins()
    }
    fn included_packs(&self) -> Vec<Arc<dyn CapabilityPack>> {
        self.included.clone()
    }
    fn skill_sources(&self) -> Vec<CapabilitySkillSource> {
        self.original.skill_sources()
    }
    fn registry_skill_sources(&self) -> Vec<CapabilitySkillSource> {
        self.original.registry_skill_sources()
    }
    fn for_framework_scope(&self, scope: &str) -> Option<Arc<dyn CapabilityPack>> {
        Some(Arc::new(Self {
            original: self
                .original
                .for_framework_scope(scope)
                .unwrap_or_else(|| self.original.clone()),
            included: self.included.clone(),
        }))
    }
    fn version_bindings(&self) -> Vec<CapabilityVersionBinding> {
        self.original.version_bindings()
    }
    fn protect_task(&self, run: RunId) -> futures::future::BoxFuture<'_, Result<(), AgentError>> {
        self.original.protect_task(run)
    }
    fn release_task(&self, run: RunId) -> futures::future::BoxFuture<'_, Result<(), AgentError>> {
        self.original.release_task(run)
    }
}

/// Framework-owned plugin installation blueprint.
#[derive(Clone)]
pub(crate) struct FrameworkAssembly {
    plugins: Vec<Arc<dyn Plugin>>,
    factories: Vec<ConfiguredPluginFactory>,
    capability_packs: Vec<Arc<dyn CapabilityPack>>,
    capability_skills_root: Option<PathBuf>,
    default_models: bool,
}
impl Default for FrameworkAssembly {
    fn default() -> Self {
        Self {
            plugins: Vec::new(),
            factories: Vec::new(),
            capability_packs: Vec::new(),
            capability_skills_root: None,
            default_models: true,
        }
    }
}

impl FrameworkAssembly {
    pub fn default_models(mut self, enabled: bool) -> Self {
        self.default_models = enabled;
        self
    }
    pub fn plugin(mut self, plugin: impl Plugin + 'static) -> Self {
        self.plugins.push(Arc::new(plugin));
        self
    }
    pub fn plugin_arc(mut self, plugin: Arc<dyn Plugin>) -> Self {
        self.plugins.push(plugin);
        self
    }

    pub fn capability_pack(mut self, pack: impl CapabilityPack + 'static) -> Self {
        self.capability_packs.push(Arc::new(pack));
        self
    }

    /// Set the application-managed root used to prepare embedded `skills/`
    /// resources from selected capability packages.
    pub fn capability_skills_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.capability_skills_root = Some(root.into());
        self
    }

    pub fn plugin_factory(mut self, factory: ConfiguredPluginFactory) -> Self {
        self.factories.push(factory);
        self
    }
    pub fn capability_pack_arc(mut self, pack: Arc<dyn CapabilityPack>) -> Self {
        self.capability_packs.push(pack);
        self
    }
    fn bind_scope(&mut self, scope: String) -> Result<(), AgentError> {
        selected_pack_closure(&self.capability_packs)?;
        fn bind(
            pack: &Arc<dyn CapabilityPack>,
            scope: &str,
            cache: &mut BTreeMap<String, Arc<dyn CapabilityPack>>,
        ) -> Arc<dyn CapabilityPack> {
            if let Some(bound) = cache.get(pack.id()) {
                return bound.clone();
            }
            let original = pack
                .for_framework_scope(scope)
                .unwrap_or_else(|| pack.clone());
            let included = original
                .included_packs()
                .iter()
                .map(|child| bind(child, scope, cache))
                .collect();
            let bound: Arc<dyn CapabilityPack> =
                Arc::new(BoundCapabilityPack { original, included });
            cache.insert(pack.id().to_owned(), bound.clone());
            bound
        }
        let mut cache = BTreeMap::new();
        self.capability_packs = self
            .capability_packs
            .iter()
            .map(|pack| bind(pack, &scope, &mut cache))
            .collect();
        Ok(())
    }
    pub(crate) fn installed_packs(&self) -> Result<Vec<Arc<dyn CapabilityPack>>, AgentError> {
        Ok(selected_pack_closure(&self.capability_packs)?
            .into_values()
            .collect())
    }
    pub(crate) fn prepare_framework(&mut self) -> Result<(), AgentError> {
        self.bind_scope(RunId::new_v4().to_string())?;
        self.preflight()
    }
    fn preflight(&self) -> Result<(), AgentError> {
        for pack in selected_pack_closure(&self.capability_packs)?.values() {
            pack.preflight()?;
        }
        let mut plugins = self.plugins.clone();
        plugins.extend(self.factories.iter().map(ConfiguredPluginFactory::plugin));
        for pack in selected_pack_closure(&self.capability_packs)?.values() {
            plugins.extend(pack.plugins());
        }
        validate_capability_packs_with_plugins(
            &self.capability_packs,
            &plugins,
            &HashSet::from(["builtin.framework-defaults".into()]),
        )
    }
    pub(crate) async fn install_plugins(mut self) -> Result<PluginContainer, AgentError> {
        self.plugins
            .extend(self.factories.iter().map(ConfiguredPluginFactory::plugin));
        for pack in selected_pack_closure(&self.capability_packs)?.values() {
            self.plugins.extend(pack.plugins());
        }
        let skill_sources = selected_capability_skill_sources(&self.capability_packs)?;
        let capability_skill_provider =
            selected_capability_requires_skills_runtime(&self.capability_packs)?;
        let capability_skills_runtime =
            selected_capability_provides_skills_runtime(&self.capability_packs)?;
        if !skill_sources.is_empty() || capability_skill_provider {
            let has_skills_runtime = self
                .plugins
                .iter()
                .any(|plugin| plugin.id() == "skills.default");
            for (index, source) in skill_sources.into_iter().enumerate() {
                self.plugins.push(Arc::new(
                    eliteforge_agent_official_plugin_skills::CapabilitySkillsSourcePlugin::new(
                        format!("builtin.capability-skills.{}.{}", source.pack_id, index),
                        source.pack_id,
                        source.source,
                        self.capability_skills_root.clone(),
                    ),
                ));
            }
            if !has_skills_runtime && !capability_skills_runtime {
                self.plugins.push(Arc::new(
                    eliteforge_agent_official_plugin_skills::SkillsPlugin::sources_only(),
                ));
            }
        }
        let mut occupied_plugin_ids = HashSet::with_capacity(self.plugins.len() + 1);
        for (index, plugin) in self.plugins.iter().enumerate() {
            let id = plugin.id().to_owned();
            if id.trim().is_empty() {
                return Err(PluginError::EmptyPluginId { index }.into());
            }
            if !occupied_plugin_ids.insert(id.clone()) {
                return Err(PluginError::DuplicatePluginId { id }.into());
            }
        }
        // DefaultsPlugin joins the same graph below. Its ID must be reserved
        // before capability factories run, but cannot satisfy a capability
        // dependency because DefaultsPlugin is ordered after all capabilities.
        let defaults_plugin_id = "builtin.framework-defaults".to_owned();
        if occupied_plugin_ids.contains(&defaults_plugin_id) {
            return Err(PluginError::DuplicatePluginId {
                id: defaults_plugin_id,
            }
            .into());
        }
        let reserved_plugin_ids = HashSet::from([defaults_plugin_id]);
        let capability_plugins = resolve_capability_packs_with_plugins(
            &self.capability_packs,
            &self.plugins,
            &reserved_plugin_ids,
        )?;
        self.plugins.extend(capability_plugins);
        let after = self.plugins.iter().map(|p| p.id().to_owned()).collect();
        self.plugins.push(Arc::new(crate::plugins::DefaultsPlugin {
            after,
            default_models: self.default_models,
        }));
        PluginContainer::build(self.plugins)
            .await
            .map_err(Into::into)
    }
}
