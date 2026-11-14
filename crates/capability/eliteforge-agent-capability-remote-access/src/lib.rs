//! Native capability pack for remote-access management.
//!
//! This pack lets a PersonAI product reuse the agent-facing control plane of its
//! remote access (mobile pairing, token rotation, enable/disable) across
//! products. The host keeps the listener, configuration storage and secrets;
//! the pack contributes one validated tool, a bundled skill and a stable pack
//! ID that products can select in their own agent runtime.
//!
//! Security rules enforced here:
//!
//! - Pairing tokens never enter the agent context. Token-like fields are masked
//!   and pairing URIs are removed before the result is returned.
//! - `rotate`, `enable` and `disable` require `confirm: true`, so an unconfirmed
//!   model call cannot change product state.
//! - The host remains the only writer of configuration and tokens.

use std::sync::Arc;

use eliteforge_agent_core::{
    CapabilityPackBuilder, CapabilitySkillSource, ToolProvider, ToolProviderPlugin,
};

mod host;
mod tool;

pub use host::RemoteAccessHost;
pub use tool::{
    RemoteAccessToolProvider, TOOL_CALL_NAME, TOOL_DESCRIPTION, TOOL_ID, input_schema,
    tool_descriptor,
};

/// Stable runtime capability-pack ID selected by products.
pub const PACK_ID: &str = "framework.remote-access";
/// Plugin ID of the `ToolProviderPlugin` registered by this pack.
pub const TOOL_PROVIDER_ID: &str = "framework.remote-access.tools";
/// Skill directory embedded from `skills/remote-access/`.
pub const SKILL_NAME: &str = "remote-access";

/// Skills bundled with the native package, independent of the source checkout.
pub fn skill_source() -> CapabilitySkillSource {
    eliteforge_agent_core::capability_skills!()
}

/// Skill names derived from the same embedded bundle used by the runtime.
pub fn skill_names() -> Vec<&'static str> {
    let CapabilitySkillSource::Embedded(bundle) = skill_source() else {
        unreachable!("remote-access skills are embedded");
    };
    bundle
        .files
        .iter()
        .filter_map(|file| file.path.strip_suffix("/SKILL.md"))
        .collect()
}

/// Entry point for products that expose remote-access management to agents.
///
/// The host supplies the product-specific configuration and persistence
/// adapter; this type only builds the framework declaration.
pub struct RemoteAccessCapability {
    host: Arc<dyn RemoteAccessHost>,
}

impl RemoteAccessCapability {
    pub fn new(host: Arc<dyn RemoteAccessHost>) -> Self {
        Self { host }
    }

    /// Tool provider contributed by the pack.
    ///
    /// Useful for catalogue/conflict checks before a framework is built.
    pub fn tool_provider(&self) -> Arc<dyn ToolProvider> {
        Arc::new(RemoteAccessToolProvider::new(self.host.clone()))
    }

    /// Build the capability-pack declaration.
    ///
    /// The builder is inert: no host method runs and no tool is leased until the
    /// framework selects this pack for an agent.
    pub fn capability_pack(&self) -> CapabilityPackBuilder<()> {
        CapabilityPackBuilder::new(PACK_ID, ())
            .skills(skill_source())
            .plugin(ToolProviderPlugin::from_arc(
                TOOL_PROVIDER_ID,
                self.tool_provider(),
            ))
    }
}
