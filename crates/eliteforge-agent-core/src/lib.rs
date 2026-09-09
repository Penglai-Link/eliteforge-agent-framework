//! Shared Agent contracts and portable registration/context primitives.
pub mod budget;
pub mod content;
pub mod context;
pub mod conversation;
pub mod dag;
pub mod errors;
pub mod hooks;
pub mod interaction;
pub mod long_term;
pub mod middleware;
pub mod models;
pub mod observers;
pub mod output;
pub mod plugins;
pub mod state;
pub mod tokens;
pub mod types;
pub mod workflow;
pub use content::*;
pub use context::*;
pub use conversation::*;
pub use dag::*;
pub use eliteforge_agent_plugin_kernel::{Plugin, PluginContext, PluginError};
pub use errors::*;
pub use hooks::*;
pub use interaction::*;
pub use long_term::*;
pub use middleware::*;
pub use models::*;
pub use output::*;
pub use plugins::*;
pub use state::*;
pub use tokens::*;
pub use types::*;
pub use workflow::*;

mod capabilities;
pub use capabilities::*;

/// Minimal host-independent runtime surface used by presentation plugins.
pub trait AgentRuntime {
    type Handle;
    fn service<T: Send + Sync + 'static>(&self) -> Option<std::sync::Arc<T>>;
    fn start(&self, request: RunRequest) -> Result<Self::Handle, AgentError>;
}
