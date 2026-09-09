//! Shared Agent contracts and portable registration/context primitives.
pub mod budget;
pub mod capability_packs;
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
pub mod operations;
pub mod output;
pub mod plugins;
pub mod state;
pub mod tokens;
pub mod types;
pub mod workflow;
pub use capability_packs::*;
pub use content::*;
pub use context::*;
pub use conversation::*;
pub use dag::*;
pub use eliteforge_agent_plugin_kernel::{
    Plugin, PluginContext, PluginError, PluginPack, PluginPackBuilder,
};
pub use errors::*;
pub use hooks::*;
pub use interaction::*;
pub use long_term::*;
pub use middleware::*;
pub use models::*;
pub use operations::*;
pub use output::*;
pub use plugins::*;
pub use state::*;
pub use tokens::*;
pub use types::*;
pub use workflow::*;

/// Internal implementation dependencies used by framework-generated code.
/// Application code should use the public `#[capability]` macro instead.
#[doc(hidden)]
pub mod __private {
    pub use inventory;
}

pub use eliteforge_agent_capability_macros::capability;

mod capabilities;
pub use capabilities::*;
