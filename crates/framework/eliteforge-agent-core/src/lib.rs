//! Shared Agent contracts and portable registration/context primitives.
extern crate self as eliteforge_agent_core;

pub mod annotations;
pub use annotations::*;
pub mod budget;
pub mod chunking;
pub use chunking::*;
pub mod retrieval_evaluation;
pub use retrieval_evaluation::*;
pub mod capability_packs;
pub mod collaboration;
pub mod content;
pub mod context;
pub mod context_preparation;
pub mod continuation_budget;
pub mod conversation;
pub mod errors;
pub mod external;
mod external_tools;
pub use external_tools::*;
pub mod history;
pub mod hooks;
pub mod interaction;
pub mod lifecycle;
pub mod long_term;
pub mod middleware;
pub mod models;
pub mod observers;
pub mod operations;
pub mod output;
pub mod plugins;
pub mod session;
pub mod skills;
pub mod state;
pub mod tokenization;
pub use tokenization::*;
pub mod tokens;
pub mod tool_batch;
mod tool_display;
pub use tool_display::*;
pub use tool_batch::*;
mod react;
pub mod tool_gateway;
pub mod transaction;
pub mod types;
pub use capability_packs::*;
pub use collaboration::*;
pub use content::*;
pub use context::*;
pub use context_preparation::*;
pub use continuation_budget::*;
pub use conversation::*;
pub use eliteforge_agent_plugin_core::{
    ExtensionBinding, ExtensionMultiplicity, ExtensionPlugin, ExtensionPoint, Plugin,
    PluginContext, PluginError, PluginReadyContext,
};
pub use errors::*;
pub use external::*;
pub use history::*;
pub use hooks::*;
pub use interaction::*;
pub use lifecycle::*;
pub use long_term::*;
pub use middleware::*;
pub use models::*;
pub use operations::*;
pub use output::*;
pub use plugins::*;
pub use react::*;
pub use session::*;
pub use skills::*;
pub use state::*;
mod telemetry;
pub use telemetry::*;
pub use tokens::*;
pub use tool_gateway::*;
pub use transaction::*;
pub use types::*;

/// Internal implementation dependencies used by framework-generated code.
/// Application code should use the public `#[capability]` macro instead.
#[doc(hidden)]
pub mod __private {
    pub use inventory;
}

pub use eliteforge_agent_capability_macros::{
    capability, capability_factory, capability_skills, capability_skills_build, extension_point,
};

mod capabilities;
pub use capabilities::*;

mod configured_factory;
pub use configured_factory::*;

mod auto_routing;
pub use auto_routing::*;
