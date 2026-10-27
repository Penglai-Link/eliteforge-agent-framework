//! Capability-pack discovery and materialization.
//!
//! A capability descriptor is inert static metadata registered by the
//! `#[capability]` macro.  Selecting a pack is the only action that permits its
//! factory to run.  The resolver deliberately validates the whole selected
//! graph before constructing the first business object.

use std::{
    any::{Any, TypeId},
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

use async_trait::async_trait;
use eliteforge_agent_plugin_core::{
    ExtensionPoint, Plugin, PluginContext, PluginError, PluginReadyContext, plugin_dependency_order,
};
use thiserror::Error;

/// Type-erased configuration held by one selected capability pack.
pub type CapabilityConfig = dyn Any + Send + Sync;

/// A business object created for one Agent build.
pub type CapabilityInstance = Arc<dyn Any + Send + Sync>;

/// Creates one business object from the configuration of its selected pack.
///
/// Factories must be synchronous in the first capability-pack release.  They
/// are called only after all discoverable, static conflicts have been checked.
pub type CapabilityFactory = fn(&CapabilityConfig) -> Result<CapabilityInstance, CapabilityError>;

/// Adapts a shared business object into an ordinary plugin.
pub type CapabilityAdapter = fn(CapabilityInstance) -> Result<Arc<dyn Plugin>, CapabilityError>;

/// Converts an extension-point implementation into the plugin shape consumed
/// by the framework.
///
/// `#[extension_point]` supplies the generic implementation. Built-in
/// contracts override it so aliases still register their legacy service in
/// addition to the extension registry.
pub trait CapabilityExtensionAdapter: ExtensionPoint {
    #[doc(hidden)]
    fn into_plugin(id: &str, implementation: Arc<Self>) -> Arc<dyn Plugin>;
}

/// One file embedded in a published capability package's `skills/` tree.
#[derive(Clone, Copy)]
pub struct CapabilitySkillFile {
    pub path: &'static str,
    pub contents: &'static [u8],
}

/// Compile-time skill resources carried by a capability package.
///
/// `content_hash` is normally calculated by the packaging macro. Resource
/// preparation recomputes the digest from these files, validates a supplied
/// SHA-256 value, and uses the recomputed digest in the application-managed
/// resource cache path.
#[derive(Clone, Copy)]
pub struct EmbeddedCapabilitySkills {
    pub files: &'static [CapabilitySkillFile],
    pub content_hash: &'static str,
}

impl EmbeddedCapabilitySkills {
    pub const fn new(files: &'static [CapabilitySkillFile], content_hash: &'static str) -> Self {
        Self {
            files,
            content_hash,
        }
    }
}

/// A skill source selected together with a capability package.
#[derive(Clone)]
pub enum CapabilitySkillSource {
    /// A directory managed by the application, useful during local development.
    Directory(PathBuf),
    /// Resources included in the capability crate's published artifact.
    Embedded(EmbeddedCapabilitySkills),
}

impl CapabilitySkillSource {
    pub fn directory(path: impl Into<PathBuf>) -> Self {
        Self::Directory(path.into())
    }

    pub const fn embedded(bundle: EmbeddedCapabilitySkills) -> Self {
        Self::Embedded(bundle)
    }
}

/// One skill source paired with the selected capability package that owns it.
#[derive(Clone)]
pub struct SelectedCapabilitySkillSource {
    pub pack_id: String,
    pub source: CapabilitySkillSource,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FactoryIdentity {
    implementation: TypeId,
    configuration: TypeId,
    factory: TypeId,
}

fn factory_identity<T: 'static, C: 'static, F: 'static>() -> FactoryIdentity {
    FactoryIdentity {
        implementation: TypeId::of::<T>(),
        configuration: TypeId::of::<C>(),
        factory: TypeId::of::<F>(),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum InstanceKey {
    Named(&'static str),
    Type {
        pack: &'static str,
        implementation: TypeId,
    },
}

/// Static metadata emitted by `#[capability]` and collected across linked
/// crates.  It does not construct a business object during registration.
#[derive(Clone, Copy)]
pub struct CapabilityDescriptor {
    /// Owning capability-pack ID.
    pub pack: &'static str,
    /// Globally unique capability/plugin ID.
    pub id: &'static str,
    /// Explicit shared-instance key, or a diagnostic label for an implicit key.
    /// Macro-generated implicit keys use the actual implementation type and pack.
    pub instance: &'static str,
    /// Diagnostic declaration of the configuration and factory shape. Generated
    /// declarations compare canonical types, not the spelling of this label.
    pub factory_signature: &'static str,
    /// Capability or ordinary-plugin IDs that must be installed first.
    pub dependencies: &'static [&'static str],
    /// Creates the business object once per selected Agent build.
    pub factory: CapabilityFactory,
    /// Converts the shared object into this descriptor's plugin role.
    pub adapter: CapabilityAdapter,
    type_identity: Option<fn() -> FactoryIdentity>,
    default_instance: bool,
    requires_skills_runtime: bool,
    provides_skills_runtime: bool,
}

impl CapabilityDescriptor {
    /// Construct a descriptor suitable for `inventory::submit!`.
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        pack: &'static str,
        id: &'static str,
        instance: &'static str,
        factory_signature: &'static str,
        dependencies: &'static [&'static str],
        factory: CapabilityFactory,
        adapter: CapabilityAdapter,
    ) -> Self {
        Self {
            pack,
            id,
            instance,
            factory_signature,
            dependencies,
            factory,
            adapter,
            type_identity: None,
            default_instance: false,
            requires_skills_runtime: false,
            provides_skills_runtime: false,
        }
    }

    /// Record canonical implementation, configuration and function-item types.
    /// The factory is never called here. Generated code passes the function item
    /// without coercing it to a function pointer, preserving its unique type.
    #[doc(hidden)]
    pub const fn with_type_identity<T: 'static, C: 'static, F: Copy + 'static>(
        mut self,
        _factory: F,
        default_instance: bool,
    ) -> Self {
        self.type_identity = Some(factory_identity::<T, C, F>);
        self.default_instance = default_instance;
        self
    }

    /// Mark a declaration that contributes a `SkillProvider`.
    ///
    /// The host can install one aggregate Skills runtime before the descriptor
    /// plugin is materialized, without constructing the provider while it
    /// discovers selected capability packages.
    #[doc(hidden)]
    pub const fn with_skills_runtime(mut self) -> Self {
        self.requires_skills_runtime = true;
        self
    }

    /// Mark a factory that creates the aggregate Skills runtime itself.
    ///
    /// This lets a configured `SkillsPlugin` own selection while capability
    /// package resource sources still register before `ready` aggregates them.
    #[doc(hidden)]
    pub const fn with_provided_skills_runtime(mut self) -> Self {
        self.provides_skills_runtime = true;
        self
    }

    fn instance_key(&self) -> InstanceKey {
        if let Some(identity) = self.type_identity.filter(|_| self.default_instance) {
            InstanceKey::Type {
                pack: self.pack,
                implementation: identity().implementation,
            }
        } else {
            InstanceKey::Named(self.instance)
        }
    }

    fn same_factory(&self, other: &Self) -> bool {
        match (self.type_identity, other.type_identity) {
            (Some(left), Some(right)) => left() == right(),
            (None, None) => self.factory_signature == other.factory_signature,
            _ => false,
        }
    }
}

inventory::collect!(CapabilityDescriptor);

/// A selected group of discoverable capability declarations.
///
/// Packs contain configuration and explicit child packs, but do not create
/// business objects themselves.  The latter happens in
/// [`resolve_capability_packs`], separately for each Agent build.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CapabilityVersionBinding {
    pub package: String,
    pub version: String,
    pub digest: String,
}

pub trait CapabilityPack: Send + Sync {
    /// Stable pack ID used by descriptors and explicit child-pack selection.
    fn id(&self) -> &str;

    /// Type-erased configuration consumed by descriptor factories in this pack.
    fn configuration(&self) -> &CapabilityConfig;

    /// Validate inert selection/configuration before any member acquires resources.
    fn preflight(&self) -> Result<(), crate::AgentError> {
        Ok(())
    }

    /// External manifests can declare SkillProviders without static inventory entries.
    fn requires_skills_runtime(&self) -> bool {
        false
    }

    /// Explicit configured plugins. Returned values must not acquire resources.
    fn plugins(&self) -> Vec<Arc<dyn Plugin>> {
        Vec::new()
    }

    /// Bind package resources to the shared Framework environment.
    fn for_framework_scope(&self, _scope: &str) -> Option<Arc<dyn CapabilityPack>> {
        None
    }

    fn version_bindings(&self) -> Vec<CapabilityVersionBinding> {
        Vec::new()
    }
    fn protect_task(
        &self,
        _run: crate::RunId,
    ) -> futures::future::BoxFuture<'_, Result<(), crate::AgentError>> {
        Box::pin(async { Ok(()) })
    }
    fn release_task(
        &self,
        _run: crate::RunId,
    ) -> futures::future::BoxFuture<'_, Result<(), crate::AgentError>> {
        Box::pin(async { Ok(()) })
    }

    /// Child packs explicitly included in this pack.
    ///
    /// Discovery alone never enables a pack; only roots supplied by the
    /// application and this explicit closure are selected.
    fn included_packs(&self) -> Vec<Arc<dyn CapabilityPack>> {
        Vec::new()
    }

    /// Skills contributed by this package when it is explicitly selected.
    fn skill_sources(&self) -> Vec<CapabilitySkillSource> {
        Vec::new()
    }

    /// Sources for host-side skill registration. A pack with its own dynamic
    /// SkillProvider can expose its managed files here without registering a
    /// second runtime provider for the same skill.
    fn registry_skill_sources(&self) -> Vec<CapabilitySkillSource> {
        self.skill_sources()
    }
}

/// Small, allocation-free-to-use builder for ordinary application packs.
///
/// The builder is itself a [`CapabilityPack`], so it can be passed directly to
/// `FrameworkBuilder::capability_pack`.  [`Self::build`] is available when an
/// explicit finalizer reads better at a call site.
pub struct CapabilityPackBuilder<C> {
    id: String,
    configuration: C,
    included_packs: Vec<Arc<dyn CapabilityPack>>,
    skill_sources: Vec<CapabilitySkillSource>,
    plugins: Vec<Arc<dyn Plugin>>,
    factories: Vec<crate::ConfiguredPluginFactory>,
}

impl<C> CapabilityPackBuilder<C>
where
    C: Any + Send + Sync,
{
    pub fn new(id: impl Into<String>, configuration: C) -> Self {
        Self {
            id: id.into(),
            configuration,
            included_packs: Vec::new(),
            skill_sources: Vec::new(),
            plugins: Vec::new(),
            factories: Vec::new(),
        }
    }

    /// Include a child pack in this pack's explicit selection closure.
    pub fn include(self, pack: impl CapabilityPack + 'static) -> Self {
        self.include_arc(Arc::new(pack))
    }

    /// Include an already shared child pack in this pack's selection closure.
    pub fn include_arc(mut self, pack: Arc<dyn CapabilityPack>) -> Self {
        self.included_packs.push(pack);
        self
    }

    /// Alias for [`Self::include`] matching the Agent builder's terminology.
    pub fn capability_pack(self, pack: impl CapabilityPack + 'static) -> Self {
        self.include(pack)
    }

    /// Add a skill source to this capability package.
    pub fn skills(mut self, source: CapabilitySkillSource) -> Self {
        self.skill_sources.push(source);
        self
    }

    /// Add an application-managed `skills/` directory to this package.
    pub fn skills_directory(self, directory: impl Into<PathBuf>) -> Self {
        self.skills(CapabilitySkillSource::directory(directory))
    }

    /// Include a configured plugin installed once by the owning Framework.
    pub fn plugin(self, plugin: impl Plugin + 'static) -> Self {
        self.plugin_arc(Arc::new(plugin))
    }
    pub fn plugin_arc(mut self, plugin: Arc<dyn Plugin>) -> Self {
        self.plugins.push(plugin);
        self
    }
    /// Declare metadata before acquiring resources; create a fresh instance during install.
    pub fn plugin_factory(mut self, factory: crate::ConfiguredPluginFactory) -> Self {
        self.factories.push(factory);
        self
    }

    /// Finish this declaration.  The returned value still implements
    /// [`CapabilityPack`] and does not construct any business resource.
    pub fn build(self) -> Self {
        self
    }
}

impl<C> CapabilityPack for CapabilityPackBuilder<C>
where
    C: Any + Send + Sync,
{
    fn id(&self) -> &str {
        &self.id
    }

    fn configuration(&self) -> &CapabilityConfig {
        &self.configuration
    }

    fn included_packs(&self) -> Vec<Arc<dyn CapabilityPack>> {
        self.included_packs.clone()
    }

    fn skill_sources(&self) -> Vec<CapabilitySkillSource> {
        self.skill_sources.clone()
    }
    fn plugins(&self) -> Vec<Arc<dyn Plugin>> {
        self.plugins
            .iter()
            .cloned()
            .chain(self.factories.iter().map(|f| f.plugin()))
            .collect()
    }
}

/// Collect skill sources from the explicit selected-package closure.
///
/// Linking a crate does not make its skills visible; roots and `include(...)`
/// are the only selection path, just as they are for code capabilities.
pub fn selected_capability_skill_sources(
    packs: &[Arc<dyn CapabilityPack>],
) -> Result<Vec<SelectedCapabilitySkillSource>, CapabilityError> {
    Ok(selected_pack_closure(packs)?
        .into_iter()
        .flat_map(|(pack_id, pack)| {
            pack.skill_sources()
                .into_iter()
                .map(move |source| SelectedCapabilitySkillSource {
                    pack_id: pack_id.clone(),
                    source,
                })
        })
        .collect())
}

/// Whether selected declarations contribute a storage-neutral `SkillProvider`.
///
/// This inspects only static descriptor metadata. It never constructs a
/// capability instance or starts provider discovery.
pub fn selected_capability_requires_skills_runtime(
    packs: &[Arc<dyn CapabilityPack>],
) -> Result<bool, CapabilityError> {
    let selected = selected_pack_closure(packs)?;
    Ok(selected.values().any(|pack| pack.requires_skills_runtime())
        || inventory::iter::<CapabilityDescriptor>
            .into_iter()
            .any(|descriptor| {
                descriptor.requires_skills_runtime && selected.contains_key(descriptor.pack)
            }))
}

/// Whether selected factories already provide the aggregate Skills runtime.
///
/// The result is static descriptor metadata and never constructs a plugin.
pub fn selected_capability_provides_skills_runtime(
    packs: &[Arc<dyn CapabilityPack>],
) -> Result<bool, CapabilityError> {
    let selected = selected_pack_closure(packs)?;
    Ok(inventory::iter::<CapabilityDescriptor>
        .into_iter()
        .any(|descriptor| {
            descriptor.provides_skills_runtime && selected.contains_key(descriptor.pack)
        }))
}

/// Errors that can be reported without installing a capability plugin.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CapabilityError {
    #[error("capability pack ID cannot be empty")]
    EmptyPackId,
    #[error("capability pack `{pack}` was selected more than once")]
    DuplicatePack { pack: String },
    #[error("capability pack inclusion cycle: {packs:?}")]
    PackCycle { packs: Vec<String> },
    #[error("capability in pack `{pack}` has an empty ID")]
    EmptyCapabilityId { pack: String },
    #[error("capability `{capability}` has an empty shared-instance key")]
    EmptyInstance { capability: String },
    #[error("capability `{capability}` has an empty factory signature")]
    EmptyFactorySignature { capability: String },
    #[error(
        "capability ID `{id}` is declared by both pack `{first_pack}` and pack `{second_pack}`"
    )]
    DuplicateCapabilityId {
        id: String,
        first_pack: String,
        second_pack: String,
    },
    #[error("capability `{capability}` depends on missing capability or plugin `{dependency}`")]
    MissingDependency {
        capability: String,
        dependency: String,
    },
    #[error("capability dependency cycle; unresolved capabilities: {capabilities:?}")]
    DependencyCycle { capabilities: Vec<String> },
    #[error("capability `{capability}` conflicts with an existing or reserved plugin ID")]
    PluginIdConflict { capability: String },
    #[error(
        "shared instance `{instance}` has incompatible factories: capability `{first_capability}` declares `{first_signature}`, but `{second_capability}` declares `{second_signature}`"
    )]
    InstanceFactoryConflict {
        instance: String,
        first_capability: String,
        first_signature: String,
        second_capability: String,
        second_signature: String,
    },
    #[error(
        "shared instance `{instance}` is declared by both pack `{first_pack}` and pack `{second_pack}`; one instance cannot choose two pack configurations"
    )]
    InstancePackConflict {
        instance: String,
        first_pack: String,
        second_pack: String,
    },
    #[error("capability `{capability}` expected pack configuration `{expected}`")]
    ConfigurationType {
        capability: String,
        expected: String,
    },
    #[error("capability `{capability}` expected shared instance `{expected}`")]
    InstanceType {
        capability: String,
        expected: String,
    },
}

impl CapabilityError {
    /// Construct the configuration downcast error used by generated factories.
    pub fn configuration_type(capability: impl Into<String>, expected: impl Into<String>) -> Self {
        Self::ConfigurationType {
            capability: capability.into(),
            expected: expected.into(),
        }
    }

    /// Construct the instance downcast error used by generated adapters.
    pub fn instance_type(capability: impl Into<String>, expected: impl Into<String>) -> Self {
        Self::InstanceType {
            capability: capability.into(),
            expected: expected.into(),
        }
    }
}

/// Resolve the selected pack closure into normal plugins for one Agent build.
///
/// The resolver performs all static validation before invoking any factory.
/// It is deterministic despite inventory's unspecified iteration order: among
/// ready descriptors, the lexicographically smallest capability ID is chosen.
pub fn resolve_capability_packs(
    packs: &[Arc<dyn CapabilityPack>],
    occupied_plugin_ids: &HashSet<String>,
) -> Result<Vec<Arc<dyn Plugin>>, CapabilityError> {
    resolve_capability_packs_with_reservations(packs, occupied_plugin_ids, &HashSet::new())
}

/// Resolve selected packs while reserving IDs that capabilities may not claim.
///
/// `occupied_plugin_ids` are ordinary plugins that are already part of the
/// same dependency graph, so a descriptor may depend on them. In contrast,
/// `reserved_plugin_ids` only protect IDs owned by a host's later internal
/// plugins; they are deliberately *not* dependency targets. This keeps a
/// capability from claiming a framework-default ID or depending on a plugin
/// that will itself be ordered after all capabilities.
/// These ID-only placeholders carry no dependency edges. Hosts with ordinary
/// plugin instances should use [`resolve_capability_packs_with_plugins`].
pub fn resolve_capability_packs_with_reservations(
    packs: &[Arc<dyn CapabilityPack>],
    occupied_plugin_ids: &HashSet<String>,
    reserved_plugin_ids: &HashSet<String>,
) -> Result<Vec<Arc<dyn Plugin>>, CapabilityError> {
    materialize_capabilities(prepare_capabilities(
        packs,
        occupied_plugin_ids,
        reserved_plugin_ids,
    )?)
}

/// Validate the combined ordinary-plugin and capability graph before invoking
/// factories. Dependencies returned by an auto-created Plugin itself can only
/// be checked after construction; its static dependencies belong in the macro.
pub fn resolve_capability_packs_with_plugins(
    packs: &[Arc<dyn CapabilityPack>],
    plugins: &[Arc<dyn Plugin>],
    reserved_plugin_ids: &HashSet<String>,
) -> Result<Vec<Arc<dyn Plugin>>, crate::AgentError> {
    materialize_capabilities(prepare_combined_capabilities(
        packs,
        plugins,
        reserved_plugin_ids,
    )?)
    .map_err(Into::into)
}
/// Validate the full declared dependency graph without calling any member factory.
pub fn validate_capability_packs_with_plugins(
    packs: &[Arc<dyn CapabilityPack>],
    plugins: &[Arc<dyn Plugin>],
    reserved_plugin_ids: &HashSet<String>,
) -> Result<(), crate::AgentError> {
    prepare_combined_capabilities(packs, plugins, reserved_plugin_ids).map(|_| ())
}
fn prepare_combined_capabilities(
    packs: &[Arc<dyn CapabilityPack>],
    plugins: &[Arc<dyn Plugin>],
    reserved_plugin_ids: &HashSet<String>,
) -> Result<PreparedCapabilities, crate::AgentError> {
    let occupied = plugins
        .iter()
        .map(|plugin| plugin.id().to_owned())
        .collect();
    let mut prepared = prepare_capabilities(packs, &occupied, reserved_plugin_ids)?;
    let declarations = plugins
        .iter()
        .map(|plugin| (plugin.id().to_owned(), plugin.dependencies()))
        .chain(prepared.ordered.iter().map(|descriptor| {
            (
                descriptor.id.to_owned(),
                descriptor
                    .dependencies
                    .iter()
                    .map(|id| (*id).to_owned())
                    .collect(),
            )
        }))
        .collect::<Vec<_>>();
    let order = plugin_dependency_order(&declarations)?;
    prepared.ordered = order
        .into_iter()
        .filter_map(|index| {
            index
                .checked_sub(plugins.len())
                .map(|index| prepared.ordered[index])
        })
        .collect();
    Ok(prepared)
}

struct PreparedCapabilities {
    selected_packs: BTreeMap<String, Arc<dyn CapabilityPack>>,
    ordered: Vec<&'static CapabilityDescriptor>,
}

fn prepare_capabilities(
    packs: &[Arc<dyn CapabilityPack>],
    occupied_plugin_ids: &HashSet<String>,
    reserved_plugin_ids: &HashSet<String>,
) -> Result<PreparedCapabilities, CapabilityError> {
    let selected_packs = selected_pack_closure(packs)?;
    let mut descriptors: Vec<_> = inventory::iter::<CapabilityDescriptor>
        .into_iter()
        .filter(|descriptor| selected_packs.contains_key(descriptor.pack))
        .collect();
    descriptors.sort_by(|left, right| {
        (left.id, left.pack, left.instance, left.factory_signature).cmp(&(
            right.id,
            right.pack,
            right.instance,
            right.factory_signature,
        ))
    });

    let ordered =
        validate_and_order_descriptors(&descriptors, occupied_plugin_ids, reserved_plugin_ids)?;
    Ok(PreparedCapabilities {
        selected_packs,
        ordered,
    })
}

fn materialize_capabilities(
    PreparedCapabilities {
        selected_packs,
        ordered,
    }: PreparedCapabilities,
) -> Result<Vec<Arc<dyn Plugin>>, CapabilityError> {
    let mut instances: HashMap<InstanceKey, CapabilityInstance> = HashMap::new();
    let mut plugins = Vec::with_capacity(ordered.len());

    for descriptor in ordered {
        let key = descriptor.instance_key();
        let instance = match instances.get(&key) {
            Some(instance) => instance.clone(),
            None => {
                let pack = selected_packs
                    .get(descriptor.pack)
                    .expect("selected descriptor must have a selected pack");
                let instance = (descriptor.factory)(pack.configuration())?;
                instances.insert(key, instance.clone());
                instance
            }
        };
        let inner = (descriptor.adapter)(instance)?;
        plugins.push(Arc::new(DescriptorPlugin::new(descriptor, inner)) as Arc<dyn Plugin>);
    }

    Ok(plugins)
}

pub fn selected_pack_closure(
    roots: &[Arc<dyn CapabilityPack>],
) -> Result<BTreeMap<String, Arc<dyn CapabilityPack>>, CapabilityError> {
    let mut selected = BTreeMap::new();
    let mut visiting = Vec::new();
    for root in roots {
        collect_pack(root.clone(), &mut selected, &mut visiting)?;
    }
    Ok(selected)
}

fn collect_pack(
    pack: Arc<dyn CapabilityPack>,
    selected: &mut BTreeMap<String, Arc<dyn CapabilityPack>>,
    visiting: &mut Vec<String>,
) -> Result<(), CapabilityError> {
    let id = pack.id().to_owned();
    if id.trim().is_empty() {
        return Err(CapabilityError::EmptyPackId);
    }
    if let Some(start) = visiting.iter().position(|current| current == &id) {
        let mut cycle = visiting[start..].to_vec();
        cycle.push(id);
        return Err(CapabilityError::PackCycle { packs: cycle });
    }
    if selected.contains_key(&id) {
        return Err(CapabilityError::DuplicatePack { pack: id });
    }

    visiting.push(id.clone());
    for child in pack.included_packs() {
        collect_pack(child, selected, visiting)?;
    }
    visiting.pop();
    selected.insert(id, pack);
    Ok(())
}

fn validate_and_order_descriptors(
    descriptors: &[&'static CapabilityDescriptor],
    occupied_plugin_ids: &HashSet<String>,
    reserved_plugin_ids: &HashSet<String>,
) -> Result<Vec<&'static CapabilityDescriptor>, CapabilityError> {
    let mut by_id = BTreeMap::new();
    let mut by_instance: BTreeMap<InstanceKey, &CapabilityDescriptor> = BTreeMap::new();

    for descriptor in descriptors {
        if descriptor.id.trim().is_empty() {
            return Err(CapabilityError::EmptyCapabilityId {
                pack: descriptor.pack.into(),
            });
        }
        if descriptor.instance.trim().is_empty() {
            return Err(CapabilityError::EmptyInstance {
                capability: descriptor.id.into(),
            });
        }
        if descriptor.factory_signature.trim().is_empty() {
            return Err(CapabilityError::EmptyFactorySignature {
                capability: descriptor.id.into(),
            });
        }
        if occupied_plugin_ids.contains(descriptor.id)
            || reserved_plugin_ids.contains(descriptor.id)
        {
            return Err(CapabilityError::PluginIdConflict {
                capability: descriptor.id.into(),
            });
        }
        if let Some(existing) = by_id.insert(descriptor.id, *descriptor) {
            return Err(CapabilityError::DuplicateCapabilityId {
                id: descriptor.id.into(),
                first_pack: existing.pack.into(),
                second_pack: descriptor.pack.into(),
            });
        }
        if let Some(existing) = by_instance.insert(descriptor.instance_key(), *descriptor) {
            if !existing.same_factory(descriptor) {
                return Err(CapabilityError::InstanceFactoryConflict {
                    instance: descriptor.instance.into(),
                    first_capability: existing.id.into(),
                    first_signature: existing.factory_signature.into(),
                    second_capability: descriptor.id.into(),
                    second_signature: descriptor.factory_signature.into(),
                });
            }
            if existing.pack != descriptor.pack {
                return Err(CapabilityError::InstancePackConflict {
                    instance: descriptor.instance.into(),
                    first_pack: existing.pack.into(),
                    second_pack: descriptor.pack.into(),
                });
            }
        }
    }

    let capability_ids: BTreeSet<_> = by_id.keys().copied().collect();
    for descriptor in descriptors {
        for dependency in descriptor.dependencies {
            if !capability_ids.contains(dependency) && !occupied_plugin_ids.contains(*dependency) {
                return Err(CapabilityError::MissingDependency {
                    capability: descriptor.id.into(),
                    dependency: (*dependency).into(),
                });
            }
        }
    }

    let mut installed = BTreeSet::new();
    let mut ordered = Vec::with_capacity(descriptors.len());
    while ordered.len() < descriptors.len() {
        let next = by_id.iter().find_map(|(id, descriptor)| {
            (!installed.contains(*id)
                && descriptor
                    .dependencies
                    .iter()
                    .filter(|dependency| capability_ids.contains(*dependency))
                    .all(|dependency| installed.contains(*dependency)))
            .then_some(*descriptor)
        });
        let Some(next) = next else {
            return Err(CapabilityError::DependencyCycle {
                capabilities: by_id
                    .keys()
                    .filter(|id| !installed.contains(*id))
                    .map(|id| (*id).to_owned())
                    .collect(),
            });
        };
        installed.insert(next.id);
        ordered.push(next);
    }
    Ok(ordered)
}

/// Gives a generated capability plugin the descriptor's public identity while
/// retaining dependencies declared by both the descriptor and its inner plugin.
struct DescriptorPlugin {
    id: String,
    dependencies: Vec<String>,
    inner: Arc<dyn Plugin>,
}

impl DescriptorPlugin {
    fn new(descriptor: &CapabilityDescriptor, inner: Arc<dyn Plugin>) -> Self {
        let mut dependencies = descriptor
            .dependencies
            .iter()
            .map(|dependency| (*dependency).to_owned())
            .collect::<Vec<_>>();
        for dependency in inner.dependencies() {
            if !dependencies.contains(&dependency) {
                dependencies.push(dependency);
            }
        }
        Self {
            id: descriptor.id.into(),
            dependencies,
            inner,
        }
    }
}

#[async_trait]
impl Plugin for DescriptorPlugin {
    fn id(&self) -> &str {
        &self.id
    }

    fn dependencies(&self) -> Vec<String> {
        self.dependencies.clone()
    }

    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        self.inner.install(context).await
    }

    async fn ready(&self, context: &mut PluginReadyContext<'_>) -> Result<(), PluginError> {
        self.inner.ready(context).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestPlugin {
        dependencies: Vec<String>,
    }

    #[async_trait]
    impl Plugin for TestPlugin {
        fn id(&self) -> &str {
            "unit.inner"
        }

        fn dependencies(&self) -> Vec<String> {
            self.dependencies.clone()
        }

        async fn install(&self, _: &mut PluginContext<'_>) -> Result<(), PluginError> {
            Ok(())
        }
    }

    fn factory(_: &CapabilityConfig) -> Result<CapabilityInstance, CapabilityError> {
        Ok(Arc::new(()))
    }

    fn adapter(_: CapabilityInstance) -> Result<Arc<dyn Plugin>, CapabilityError> {
        Ok(Arc::new(TestPlugin {
            dependencies: Vec::new(),
        }))
    }

    static RESERVATION_FACTORY_CALLS: AtomicUsize = AtomicUsize::new(0);

    fn counted_factory(_: &CapabilityConfig) -> Result<CapabilityInstance, CapabilityError> {
        RESERVATION_FACTORY_CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(()))
    }

    inventory::submit! {
        CapabilityDescriptor::new(
            "unit.reservation-conflict",
            "builtin.framework-defaults",
            "unit.reservation-conflict",
            "unit.reservation-conflict.factory",
            &[],
            counted_factory,
            adapter,
        )
    }

    inventory::submit! {
        CapabilityDescriptor::new(
            "unit.reservation-dependency",
            "unit.reservation-dependency",
            "unit.reservation-dependency",
            "unit.reservation-dependency.factory",
            &["builtin.framework-defaults"],
            counted_factory,
            adapter,
        )
    }

    static FIRST: CapabilityDescriptor = CapabilityDescriptor::new(
        "unit",
        "unit.first",
        "unit.first",
        "unit.first.factory",
        &[],
        factory,
        adapter,
    );
    static SECOND: CapabilityDescriptor = CapabilityDescriptor::new(
        "unit",
        "unit.second",
        "unit.second",
        "unit.second.factory",
        &["unit.first"],
        factory,
        adapter,
    );
    static THIRD: CapabilityDescriptor = CapabilityDescriptor::new(
        "unit",
        "unit.third",
        "unit.third",
        "unit.third.factory",
        &[],
        factory,
        adapter,
    );
    static CONFLICTING_FACTORY: CapabilityDescriptor = CapabilityDescriptor::new(
        "unit",
        "unit.factory-conflict",
        "unit.shared",
        "unit.other.factory",
        &[],
        factory,
        adapter,
    );
    static SHARED_FACTORY: CapabilityDescriptor = CapabilityDescriptor::new(
        "unit",
        "unit.shared-owner",
        "unit.shared",
        "unit.shared.factory",
        &[],
        factory,
        adapter,
    );
    static WRAPPED: CapabilityDescriptor = CapabilityDescriptor::new(
        "unit",
        "unit.wrapper",
        "unit.wrapper",
        "unit.wrapper.factory",
        &["descriptor.dependency"],
        factory,
        adapter,
    );
    static RESERVED_ID: CapabilityDescriptor = CapabilityDescriptor::new(
        "unit",
        "builtin.framework-defaults",
        "unit.reserved-id",
        "unit.reserved-id.factory",
        &[],
        factory,
        adapter,
    );
    static DEPENDS_ON_RESERVED: CapabilityDescriptor = CapabilityDescriptor::new(
        "unit",
        "unit.depends-on-reserved",
        "unit.depends-on-reserved",
        "unit.depends-on-reserved.factory",
        &["builtin.framework-defaults"],
        factory,
        adapter,
    );

    #[test]
    fn topological_order_uses_capability_id_as_the_stable_tie_breaker() {
        let ordered = validate_and_order_descriptors(
            &[&THIRD, &SECOND, &FIRST],
            &HashSet::new(),
            &HashSet::new(),
        )
        .expect("the static graph is valid");
        assert_eq!(
            ordered
                .into_iter()
                .map(|descriptor| descriptor.id)
                .collect::<Vec<_>>(),
            ["unit.first", "unit.second", "unit.third"]
        );
    }

    #[test]
    fn incompatible_shared_factory_is_rejected_before_materialization() {
        let error = match validate_and_order_descriptors(
            &[&SHARED_FACTORY, &CONFLICTING_FACTORY],
            &HashSet::new(),
            &HashSet::new(),
        ) {
            Err(error) => error,
            Ok(_) => panic!("the same instance cannot use two factory signatures"),
        };
        assert!(matches!(
            error,
            CapabilityError::InstanceFactoryConflict { instance, .. } if instance == "unit.shared"
        ));
    }

    #[test]
    fn reserved_ids_cannot_be_claimed_or_used_as_dependency_targets() {
        let reserved = HashSet::from(["builtin.framework-defaults".to_owned()]);
        let conflict =
            match validate_and_order_descriptors(&[&RESERVED_ID], &HashSet::new(), &reserved) {
                Err(error) => error,
                Ok(_) => panic!("a reserved plugin ID cannot be claimed"),
            };
        assert!(matches!(
            conflict,
            CapabilityError::PluginIdConflict { capability }
                if capability == "builtin.framework-defaults"
        ));

        let missing = match validate_and_order_descriptors(
            &[&DEPENDS_ON_RESERVED],
            &HashSet::new(),
            &reserved,
        ) {
            Err(error) => error,
            Ok(_) => panic!("a reserved plugin ID cannot satisfy a dependency"),
        };
        assert!(matches!(
            missing,
            CapabilityError::MissingDependency {
                capability,
                dependency,
            } if capability == "unit.depends-on-reserved" && dependency == "builtin.framework-defaults"
        ));
    }

    #[test]
    fn reservation_preflight_never_invokes_a_factory() {
        let reserved = HashSet::from(["builtin.framework-defaults".to_owned()]);

        RESERVATION_FACTORY_CALLS.store(0, Ordering::SeqCst);
        let conflict_pack: Arc<dyn CapabilityPack> =
            Arc::new(CapabilityPackBuilder::new("unit.reservation-conflict", ()));
        let conflict = match resolve_capability_packs_with_reservations(
            &[conflict_pack],
            &HashSet::new(),
            &reserved,
        ) {
            Err(error) => error,
            Ok(_) => panic!("a reserved plugin ID cannot be claimed"),
        };
        assert!(matches!(
            conflict,
            CapabilityError::PluginIdConflict { capability }
                if capability == "builtin.framework-defaults"
        ));
        assert_eq!(RESERVATION_FACTORY_CALLS.load(Ordering::SeqCst), 0);

        let dependency_pack: Arc<dyn CapabilityPack> = Arc::new(CapabilityPackBuilder::new(
            "unit.reservation-dependency",
            (),
        ));
        let missing = match resolve_capability_packs_with_reservations(
            &[dependency_pack],
            &HashSet::new(),
            &reserved,
        ) {
            Err(error) => error,
            Ok(_) => panic!("a reserved plugin ID cannot satisfy a dependency"),
        };
        assert!(matches!(
            missing,
            CapabilityError::MissingDependency {
                capability,
                dependency,
            } if capability == "unit.reservation-dependency" && dependency == "builtin.framework-defaults"
        ));
        assert_eq!(RESERVATION_FACTORY_CALLS.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn duplicate_pack_selection_is_rejected_before_descriptor_resolution() {
        let first: Arc<dyn CapabilityPack> = Arc::new(CapabilityPackBuilder::new("unit.pack", ()));
        let duplicate: Arc<dyn CapabilityPack> =
            Arc::new(CapabilityPackBuilder::new("unit.pack", ()));
        let error = match selected_pack_closure(&[first, duplicate]) {
            Err(error) => error,
            Ok(_) => panic!("one pack ID cannot be selected twice"),
        };
        assert!(matches!(
            error,
            CapabilityError::DuplicatePack { pack } if pack == "unit.pack"
        ));
    }

    #[test]
    fn wrapper_preserves_descriptor_and_inner_dependencies() {
        let inner: Arc<dyn Plugin> = Arc::new(TestPlugin {
            dependencies: vec!["inner.dependency".into(), "descriptor.dependency".into()],
        });
        let wrapper = DescriptorPlugin::new(&WRAPPED, inner);
        assert_eq!(wrapper.id(), "unit.wrapper");
        assert_eq!(
            wrapper.dependencies(),
            ["descriptor.dependency", "inner.dependency"]
        );
    }
}
