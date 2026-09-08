//! Host-neutral execution contracts, ReAct and durable workflows assembled
//! through `agent-plugin-kernel`.
//!
//! Plugins supply models, memory, tools, context and event sinks. Business
//! ownership, permissions and configuration inheritance belong to the host.

mod agent;
mod budget;
mod content;
mod context;
mod conversation;
mod dag;
mod errors;
mod hooks;
mod interaction;
mod long_term;
mod middleware;
mod model_execution;
mod models;
mod observers;
mod output;
mod plugins;
mod react;
mod state;
mod tokens;
mod types;
mod workflow;

pub use agent::*;
pub use agent_plugin_kernel::{Plugin, PluginContext, PluginError};
pub use content::*;
pub use context::*;
pub use conversation::*;
pub use dag::*;
pub use errors::*;
pub use hooks::*;
pub use interaction::*;
pub use long_term::*;
pub use middleware::*;
pub use model_execution::TranscriptProjection;
pub use models::*;
pub use output::*;
pub use plugins::*;
pub use react::ReactAgentLoop;
pub use state::*;
pub use tokens::*;
pub use types::*;
pub use workflow::*;

#[cfg(test)]
mod tests;
