//! Official assembly for one externally managed Agent execution engine.
use std::sync::Arc;

pub use eliteforge_agent_official_plugin_agent_external::*;

pub fn external_agent_pack(
    adapter: impl eliteforge_agent_core::ExternalAgent + 'static,
) -> eliteforge_agent_core::CapabilityPackBuilder<()> {
    external_agent_pack_from_arc(Arc::new(adapter))
}

pub fn external_agent_pack_from_arc(
    adapter: Arc<dyn eliteforge_agent_core::ExternalAgent>,
) -> eliteforge_agent_core::CapabilityPackBuilder<()> {
    eliteforge_agent_core::CapabilityPackBuilder::new("framework.external-agent", ())
        .plugin(eliteforge_agent_core::ExternalAgentPlugin::from_arc(
            adapter,
        ))
        .plugin(eliteforge_agent_core::AgentLoopPlugin::new(
            eliteforge_agent_framework::ExternalAgentLoop,
        ))
}
