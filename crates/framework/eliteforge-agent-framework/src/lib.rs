//! Agent runtime with default model plugins.
pub use eliteforge_agent_core::*;
#[cfg(feature = "genai")]
pub use eliteforge_agent_official_plugin_model_genai::GenaiPlugin;
mod capability_execution;
mod external_loop;
pub use capability_execution::ModelClient;
pub use external_loop::ExternalAgentLoop;
pub mod dag;
pub use dag::*;
mod agent;
pub use agent::*;
pub mod models;
pub use models::*;
pub mod plugins;
pub use plugins::*;
mod react;
pub use react::*;
pub mod workflow;
pub use workflow::*;
mod model_execution;
mod model_recovery;
mod model_runtime;
pub use model_execution::TranscriptProjection;
pub use model_recovery::*;
pub use model_runtime::*;
#[cfg(test)]
mod tests;

mod capability_state;

pub use eliteforge_agent_capability_runtime::*;

mod model_call_budget;

mod model_observation;
pub use model_observation::{ModelExecutionReport, ObservedModelResult};
mod model_invocation;

mod continuation;
pub use continuation::*;
