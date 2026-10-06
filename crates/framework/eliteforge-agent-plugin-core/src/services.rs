use std::{
    any::{Any, TypeId, type_name},
    collections::HashMap,
    sync::Arc,
};

use crate::extensions::ExtensionRegistry;
use crate::{ExtensionPoint, PluginError};

type ErasedService = Arc<dyn Any + Send + Sync>;

#[derive(Clone)]
struct Registration {
    plugin: String,
    service: ErasedService,
}

#[derive(Clone)]
enum ServiceEntry {
    Unique(Registration),
    Contributions(Vec<Registration>),
}

/// A read-only view of installed services, keyed by concrete Rust type.
///
/// Use a newtype when two semantically different services share the same type.
/// Trait object services can be registered as `Arc<dyn MyService>`; retrieving
/// one with `get::<Arc<dyn MyService>>()` returns `Arc<Arc<dyn MyService>>`.
/// Registrations become immutable after container construction.
#[derive(Default, Clone)]
pub struct ServiceRegistry {
    entries: HashMap<TypeId, ServiceEntry>,
    extensions: ExtensionRegistry,
}

impl ServiceRegistry {
    /// Retrieve a unique service. Contribution registrations are not returned.
    pub fn get<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        match self.entries.get(&TypeId::of::<T>())? {
            ServiceEntry::Unique(registration) => registration.service.clone().downcast().ok(),
            ServiceEntry::Contributions(_) => None,
        }
    }

    /// Retrieve contributions in installation and registration order.
    /// Unique registrations are not returned.
    pub fn all<T: Send + Sync + 'static>(&self) -> Vec<Arc<T>> {
        match self.entries.get(&TypeId::of::<T>()) {
            Some(ServiceEntry::Contributions(registrations)) => registrations
                .iter()
                .filter_map(|entry| entry.service.clone().downcast().ok())
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Whether either kind of registration exists for `T`.
    pub fn contains<T: Send + Sync + 'static>(&self) -> bool {
        self.entries.contains_key(&TypeId::of::<T>())
    }

    /// Plugin IDs responsible for this type, in registration order.
    pub fn providers<T: Send + Sync + 'static>(&self) -> Vec<&str> {
        match self.entries.get(&TypeId::of::<T>()) {
            Some(ServiceEntry::Unique(registration)) => vec![registration.plugin.as_str()],
            Some(ServiceEntry::Contributions(registrations)) => registrations
                .iter()
                .map(|registration| registration.plugin.as_str())
                .collect(),
            None => Vec::new(),
        }
    }

    /// Get the implementation of an extension point declared as `single`.
    pub fn extension<T>(&self) -> Option<Arc<T>>
    where
        T: ExtensionPoint + ?Sized + 'static,
    {
        self.extensions.extension::<T>()
    }

    /// Get every implementation of an extension point in registration order.
    pub fn extensions<T>(&self) -> Vec<Arc<T>>
    where
        T: ExtensionPoint + ?Sized + 'static,
    {
        self.extensions.extensions::<T>()
    }

    /// Retrieve a named Framework candidate without assuming global uniqueness.
    pub fn extension_from<T>(&self, provider: &str) -> Option<Arc<T>>
    where
        T: ExtensionPoint + ?Sized + 'static,
    {
        self.extensions
            .providers::<T>()
            .into_iter()
            .zip(self.extensions.extensions::<T>())
            .find_map(|(id, implementation)| (id == provider).then_some(implementation))
    }

    /// IDs of plugins that registered an extension point.
    pub fn extension_providers<T>(&self) -> Vec<&str>
    where
        T: ExtensionPoint + ?Sized + 'static,
    {
        self.extensions.providers::<T>()
    }

    /// Resolve an Agent binding without changing the Framework registry.
    /// A selected contribution becomes a unique service in this local view.
    pub fn bind_one<T: Send + Sync + 'static>(
        &mut self,
        provider: Option<&str>,
    ) -> Result<(), PluginError> {
        let key = TypeId::of::<T>();
        let candidates = match self.entries.get(&key) {
            Some(ServiceEntry::Unique(entry)) => vec![entry.clone()],
            Some(ServiceEntry::Contributions(entries)) => entries.clone(),
            None => Vec::new(),
        };
        let mut providers = std::collections::HashSet::new();
        if candidates
            .iter()
            .any(|entry| !providers.insert(entry.plugin.as_str()))
        {
            return Err(PluginError::message(format!(
                "{} has duplicate implementation IDs",
                type_name::<T>()
            )));
        }
        let selected = match provider {
            Some(id) => candidates
                .iter()
                .find(|entry| entry.plugin == id)
                .cloned()
                .ok_or_else(|| {
                    PluginError::message(format!(
                        "{} provider {id} is not installed",
                        type_name::<T>()
                    ))
                })?,
            None if candidates.len() == 1 => candidates[0].clone(),
            None if candidates.is_empty() => return Ok(()),
            None => {
                return Err(PluginError::message(format!(
                    "{} has multiple implementations; select a provider or configure a Framework default",
                    type_name::<T>()
                )));
            }
        };
        self.entries.insert(key, ServiceEntry::Unique(selected));
        Ok(())
    }

    /// Restrict an Agent's optional contributions in the requested order.
    pub fn bind_many<T: Send + Sync + 'static>(
        &mut self,
        providers: &[String],
    ) -> Result<(), PluginError> {
        let key = TypeId::of::<T>();
        let candidates = match self.entries.get(&key) {
            Some(ServiceEntry::Contributions(entries)) => entries.clone(),
            Some(ServiceEntry::Unique(entry)) => vec![entry.clone()],
            None => Vec::new(),
        };
        let mut selected = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for id in providers {
            if !seen.insert(id) {
                return Err(PluginError::message(format!(
                    "duplicate selected provider {id}"
                )));
            }
            let matches: Vec<_> = candidates
                .iter()
                .filter(|entry| &entry.plugin == id)
                .cloned()
                .collect();
            if matches.is_empty() {
                return Err(PluginError::message(format!(
                    "{} provider {id} is not installed",
                    type_name::<T>()
                )));
            }
            selected.extend(matches);
        }
        self.entries
            .insert(key, ServiceEntry::Contributions(selected));
        Ok(())
    }

    /// Omit an optional capability from an Agent's binding view.
    pub fn omit<T: Send + Sync + 'static>(&mut self) {
        self.entries.remove(&TypeId::of::<T>());
    }

    /// Transform the services in a local binding view, preserving ownership and order.
    pub fn map_services<T: Send + Sync + 'static>(&mut self, map: impl Fn(&T) -> T) {
        let update = |entry: &mut Registration| {
            if let Some(value) = entry.service.as_ref().downcast_ref::<T>() {
                entry.service = Arc::new(map(value));
            }
        };
        match self.entries.get_mut(&TypeId::of::<T>()) {
            Some(ServiceEntry::Unique(entry)) => update(entry),
            Some(ServiceEntry::Contributions(entries)) => entries.iter_mut().for_each(update),
            None => {}
        }
    }

    pub(crate) fn provide<T: Send + Sync + 'static>(
        &mut self,
        plugin: &str,
        value: T,
    ) -> Result<(), PluginError> {
        let key = TypeId::of::<T>();
        match self.entries.get(&key) {
            Some(ServiceEntry::Unique(existing)) => {
                return Err(PluginError::DuplicateService {
                    service: type_name::<T>(),
                    existing_plugin: existing.plugin.clone(),
                    plugin: plugin.to_owned(),
                });
            }
            Some(ServiceEntry::Contributions(existing)) => {
                return Err(PluginError::ServiceKindConflict {
                    service: type_name::<T>(),
                    existing_plugin: existing[0].plugin.clone(),
                    plugin: plugin.to_owned(),
                });
            }
            None => {}
        }
        self.entries.insert(
            key,
            ServiceEntry::Unique(Registration {
                plugin: plugin.to_owned(),
                service: Arc::new(value),
            }),
        );
        Ok(())
    }

    pub(crate) fn contribute<T: Send + Sync + 'static>(
        &mut self,
        plugin: &str,
        value: T,
    ) -> Result<(), PluginError> {
        let key = TypeId::of::<T>();
        match self.entries.get_mut(&key) {
            Some(ServiceEntry::Unique(existing)) => Err(PluginError::ServiceKindConflict {
                service: type_name::<T>(),
                existing_plugin: existing.plugin.clone(),
                plugin: plugin.to_owned(),
            }),
            Some(ServiceEntry::Contributions(registrations)) => {
                registrations.push(Registration {
                    plugin: plugin.to_owned(),
                    service: Arc::new(value),
                });
                Ok(())
            }
            None => {
                self.entries.insert(
                    key,
                    ServiceEntry::Contributions(vec![Registration {
                        plugin: plugin.to_owned(),
                        service: Arc::new(value),
                    }]),
                );
                Ok(())
            }
        }
    }

    pub(crate) fn contribute_extension<T>(
        &mut self,
        plugin: &str,
        value: Arc<T>,
    ) -> Result<(), PluginError>
    where
        T: ExtensionPoint + ?Sized + 'static,
    {
        self.extensions.register(plugin, value)
    }

    pub(crate) fn detach(&mut self, owners: &std::collections::HashSet<String>) -> Self {
        let mut selected = Self::default();
        for (key, entry) in std::mem::take(&mut self.entries) {
            match entry {
                ServiceEntry::Unique(registration) => {
                    if owners.contains(&registration.plugin) {
                        selected
                            .entries
                            .insert(key, ServiceEntry::Unique(registration));
                    } else {
                        self.entries.insert(key, ServiceEntry::Unique(registration));
                    }
                }
                ServiceEntry::Contributions(registrations) => {
                    let (retained, remaining): (Vec<_>, Vec<_>) = registrations
                        .into_iter()
                        .partition(|r| owners.contains(&r.plugin));
                    if !retained.is_empty() {
                        selected
                            .entries
                            .insert(key, ServiceEntry::Contributions(retained));
                    }
                    if !remaining.is_empty() {
                        self.entries
                            .insert(key, ServiceEntry::Contributions(remaining));
                    }
                }
            }
        }
        selected.extensions = self.extensions.detach(owners);
        selected
    }
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.extensions.clear();
    }
}
