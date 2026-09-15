use std::{
    any::{Any, TypeId, type_name},
    collections::HashMap,
    sync::Arc,
};

use crate::extensions::ExtensionRegistry;
use crate::{ExtensionPoint, PluginError};

type ErasedService = Arc<dyn Any + Send + Sync>;

struct Registration {
    plugin: String,
    service: ErasedService,
}

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
#[derive(Default)]
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

    /// IDs of plugins that registered an extension point.
    pub fn extension_providers<T>(&self) -> Vec<&str>
    where
        T: ExtensionPoint + ?Sized + 'static,
    {
        self.extensions.providers::<T>()
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

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.extensions.clear();
    }
}
