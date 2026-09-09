use thiserror::Error;

/// A dependency, registration, or installation failure.
#[derive(Debug, Error)]
pub enum PluginError {
    #[error("plugin at position {index} has an empty ID")]
    EmptyPluginId { index: usize },
    #[error("plugin ID `{id}` is registered more than once")]
    DuplicatePluginId { id: String },
    #[error("plugin `{plugin}` depends on missing plugin `{dependency}`")]
    MissingDependency { plugin: String, dependency: String },
    #[error("plugin dependency cycle; unresolved plugins: {plugins:?}")]
    DependencyCycle { plugins: Vec<String> },
    #[error(
        "unique service `{service}` provided by `{existing_plugin}` cannot be registered again by `{plugin}`"
    )]
    DuplicateService {
        service: &'static str,
        existing_plugin: String,
        plugin: String,
    },
    #[error(
        "service `{service}` cannot mix unique and contribution registrations (`{existing_plugin}`, `{plugin}`)"
    )]
    ServiceKindConflict {
        service: &'static str,
        existing_plugin: String,
        plugin: String,
    },
    #[error("plugin `{plugin}` failed to install: {source}")]
    InstallationFailed {
        plugin: String,
        #[source]
        source: Box<PluginError>,
    },
    #[error("{0}")]
    Message(String),
}

impl PluginError {
    /// Construct an error for a host or plugin-specific installation failure.
    pub fn message(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}
