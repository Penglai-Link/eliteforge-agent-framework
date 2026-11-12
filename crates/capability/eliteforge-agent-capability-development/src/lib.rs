//! Self-contained development guidance for EliteForge Framework and Agent UI.

use eliteforge_agent_core::{CapabilityPackBuilder, CapabilitySkillSource};

pub const PACK_ID: &str = "eliteforge.agent-development";
pub const PACK_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Embed instructions, references and templates without source-checkout dependencies.
pub fn skill_source() -> CapabilitySkillSource {
    eliteforge_agent_core::capability_skills!()
}

/// Derive catalog entries from the same bundle consumed by the runtime.
pub fn skill_names() -> Vec<&'static str> {
    let CapabilitySkillSource::Embedded(bundle) = skill_source() else {
        unreachable!("Agent development skills are embedded");
    };
    bundle
        .files
        .iter()
        .filter_map(|file| file.path.strip_suffix("/SKILL.md"))
        .collect()
}

pub fn capability_pack() -> CapabilityPackBuilder<()> {
    CapabilityPackBuilder::new(PACK_ID, ()).skills(skill_source())
}
