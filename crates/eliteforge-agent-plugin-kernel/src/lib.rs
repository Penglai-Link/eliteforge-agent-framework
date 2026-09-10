//! A small, host-neutral plugin container.
//!
//! Plugins register typed services, async shutdown and synchronous Drop fallbacks. Dependency
//! validation happens before installation; a failed or cancelled build releases
//! everything installed so far. A completed container exposes a read-only service
//! registry until it is closed or dropped.
//!
//! There are no business ownership or authorization concepts in this crate.

mod container;
mod error;
mod extensions;
mod pack;
mod resources;
mod services;

pub use async_trait::async_trait;
pub use container::{
    ExtensionPlugin, Plugin, PluginContainer, PluginContext, PluginReadyContext,
    plugin_dependency_order,
};
pub use error::PluginError;
pub use extensions::{ExtensionMultiplicity, ExtensionPoint};
pub use pack::{PluginPack, PluginPackBuilder};
pub use resources::{ResourceScope, ShutdownReport};
pub use services::ServiceRegistry;
