//! Official assembly for one externally managed Agent execution engine.
use std::sync::Arc;

pub use eliteforge_agent_official_plugin_agent_external::*;

pub fn external_agent_pack(
    adapter: impl eliteforge_agent_core::ExternalAgent + 'static,
) -> eliteforge_agent_core::CapabilityPackBuilder<()> {
    external_agent_pack_with_loop(adapter, "framework.loop")
}

pub fn external_agent_pack_from_arc(
    adapter: Arc<dyn eliteforge_agent_core::ExternalAgent>,
) -> eliteforge_agent_core::CapabilityPackBuilder<()> {
    pack_with_loop(adapter, "framework.loop")
}

/// Assemble the external driver with a host-selected loop identity.
pub fn external_agent_pack_with_loop(
    adapter: impl eliteforge_agent_core::ExternalAgent + 'static,
    loop_id: impl Into<String>,
) -> eliteforge_agent_core::CapabilityPackBuilder<()> {
    pack_with_loop(Arc::new(adapter), loop_id)
}

fn pack_with_loop(
    adapter: Arc<dyn eliteforge_agent_core::ExternalAgent>,
    loop_id: impl Into<String>,
) -> eliteforge_agent_core::CapabilityPackBuilder<()> {
    eliteforge_agent_core::CapabilityPackBuilder::new("framework.external-agent", ())
        .plugin(eliteforge_agent_core::ExternalAgentPlugin::from_arc(
            adapter,
        ))
        .plugin(eliteforge_agent_core::AgentLoopPlugin::named(
            loop_id,
            eliteforge_agent_framework::ExternalAgentLoop,
        ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use eliteforge_agent_core::CapabilityPack;
    fn adapter() -> CliExternalAgent {
        CliExternalAgent::new(
            ExternalCli::new(ExternalCliKind::Dsh, "dsh", None),
            Some("provider/model".into()),
        )
    }
    #[test]
    fn assembly_has_one_binding_and_selected_loop_without_starting_cli() {
        let pack = external_agent_pack_with_loop(adapter(), "host.external-loop");
        let plugins = pack.plugins();
        let ids: Vec<_> = plugins.iter().map(|p| p.id()).collect();
        assert_eq!(ids, ["framework.external-agent", "host.external-loop"]);
        let default_pack = external_agent_pack(adapter());
        let defaults = default_pack.plugins();
        assert_eq!(
            defaults.iter().map(|p| p.id()).collect::<Vec<_>>(),
            ["framework.external-agent", "framework.loop"]
        );
    }
}
