use std::{
    any::{Any, TypeId},
    collections::HashMap,
    sync::Arc,
};

use crate::PluginError;

/// Whether an extension point accepts one implementation or an ordered set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionMultiplicity {
    Multiple,
    Single,
}

/// A trait marked as an extensibility boundary.
///
/// `#[extension_point]` generates this implementation for a trait. The
/// associated storage type stays private to the trait's defining crate, while
/// the container keeps it type-erased until a consumer asks for the trait.
pub trait ExtensionPoint: Send + Sync {
    #[doc(hidden)]
    type Stored: Send + Sync + 'static;

    #[doc(hidden)]
    const MULTIPLICITY: ExtensionMultiplicity;

    #[doc(hidden)]
    fn into_stored(value: Arc<Self>) -> Self::Stored;

    #[doc(hidden)]
    fn from_stored(value: &Self::Stored) -> Arc<Self>;

    #[doc(hidden)]
    fn name() -> &'static str;
}

struct ExtensionRegistration {
    plugin: String,
    value: Arc<dyn Any + Send + Sync>,
}

enum ExtensionEntry {
    Single(ExtensionRegistration),
    Multiple(Vec<ExtensionRegistration>),
}

/// Internal storage for interface implementations declared with
/// `#[extension_point]`.
#[derive(Default)]
pub(crate) struct ExtensionRegistry {
    entries: HashMap<TypeId, ExtensionEntry>,
}

impl ExtensionRegistry {
    pub(crate) fn register<T>(&mut self, plugin: &str, value: Arc<T>) -> Result<(), PluginError>
    where
        T: ExtensionPoint + ?Sized + 'static,
    {
        let key = TypeId::of::<T>();
        let registration = ExtensionRegistration {
            plugin: plugin.to_owned(),
            value: Arc::new(T::into_stored(value)),
        };

        match (T::MULTIPLICITY, self.entries.get_mut(&key)) {
            (ExtensionMultiplicity::Single, Some(ExtensionEntry::Single(existing))) => {
                Err(PluginError::DuplicateExtension {
                    extension: T::name(),
                    existing_plugin: existing.plugin.clone(),
                    plugin: plugin.to_owned(),
                })
            }
            (ExtensionMultiplicity::Single, Some(ExtensionEntry::Multiple(existing))) => {
                Err(PluginError::ExtensionKindConflict {
                    extension: T::name(),
                    existing_plugin: existing[0].plugin.clone(),
                    plugin: plugin.to_owned(),
                })
            }
            (ExtensionMultiplicity::Single, None) => {
                self.entries
                    .insert(key, ExtensionEntry::Single(registration));
                Ok(())
            }
            (ExtensionMultiplicity::Multiple, Some(ExtensionEntry::Multiple(existing))) => {
                existing.push(registration);
                Ok(())
            }
            (ExtensionMultiplicity::Multiple, Some(ExtensionEntry::Single(existing))) => {
                Err(PluginError::ExtensionKindConflict {
                    extension: T::name(),
                    existing_plugin: existing.plugin.clone(),
                    plugin: plugin.to_owned(),
                })
            }
            (ExtensionMultiplicity::Multiple, None) => {
                self.entries
                    .insert(key, ExtensionEntry::Multiple(vec![registration]));
                Ok(())
            }
        }
    }

    pub(crate) fn extension<T>(&self) -> Option<Arc<T>>
    where
        T: ExtensionPoint + ?Sized + 'static,
    {
        if T::MULTIPLICITY != ExtensionMultiplicity::Single {
            return None;
        }
        let ExtensionEntry::Single(registration) = self.entries.get(&TypeId::of::<T>())? else {
            return None;
        };
        registration
            .value
            .as_ref()
            .downcast_ref::<T::Stored>()
            .map(T::from_stored)
    }

    pub(crate) fn extensions<T>(&self) -> Vec<Arc<T>>
    where
        T: ExtensionPoint + ?Sized + 'static,
    {
        let Some(entry) = self.entries.get(&TypeId::of::<T>()) else {
            return Vec::new();
        };
        let registrations: &[ExtensionRegistration] = match entry {
            ExtensionEntry::Single(registration) => std::slice::from_ref(registration),
            ExtensionEntry::Multiple(registrations) => registrations,
        };
        registrations
            .iter()
            .filter_map(|registration| {
                registration
                    .value
                    .as_ref()
                    .downcast_ref::<T::Stored>()
                    .map(T::from_stored)
            })
            .collect()
    }

    pub(crate) fn providers<T>(&self) -> Vec<&str>
    where
        T: ExtensionPoint + ?Sized + 'static,
    {
        match self.entries.get(&TypeId::of::<T>()) {
            Some(ExtensionEntry::Single(registration)) => vec![registration.plugin.as_str()],
            Some(ExtensionEntry::Multiple(registrations)) => registrations
                .iter()
                .map(|registration| registration.plugin.as_str())
                .collect(),
            None => Vec::new(),
        }
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }
}
