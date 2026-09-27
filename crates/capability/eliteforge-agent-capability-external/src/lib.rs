//! Official in-process assembly for external Agent CLI and configured bridges.
/// Runtime extension implementation description owned by this capability package.
pub const RUNTIME_IMPLEMENTATION_DESCRIPTION: &str =
    "Bridges external agent requests into a single Framework runtime step.";

pub use eliteforge_agent_official_plugin_model_external::*;
pub fn external_agent_pack() -> eliteforge_agent_core::CapabilityPackBuilder<()> {
    eliteforge_agent_core::CapabilityPackBuilder::new("framework.external-agent", ())
        .plugin(model_plugin())
}
