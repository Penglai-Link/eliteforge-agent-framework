//! Local deterministic strategies; parsing, providers and index publication belong to hosts.
use eliteforge_agent_core::*;
use std::sync::Arc;
mod splitting;
mod structural;
pub use splitting::FixedChunkStrategy;
pub use structural::{AutomaticChunkStrategy, HeadingChunkStrategy, StructuralChunkStrategy};

pub fn built_in_chunk_strategies() -> ChunkStrategies {
    let mut strategies = ChunkStrategies::default();
    for strategy in [
        Arc::new(AutomaticChunkStrategy) as Arc<dyn ChunkStrategy>,
        Arc::new(StructuralChunkStrategy),
        Arc::new(HeadingChunkStrategy),
        Arc::new(FixedChunkStrategy),
    ] {
        strategies
            .register(strategy)
            .expect("unique built-in chunk strategy identities");
    }
    strategies
}

pub struct ChunkingCapabilityPack(pub ChunkStrategies);
impl Default for ChunkingCapabilityPack {
    fn default() -> Self {
        Self(built_in_chunk_strategies())
    }
}
impl CapabilityPack for ChunkingCapabilityPack {
    fn id(&self) -> &str {
        "document-chunking"
    }
    fn configuration(&self) -> &CapabilityConfig {
        &()
    }
    fn plugins(&self) -> Vec<Arc<dyn Plugin>> {
        vec![Arc::new(ChunkStrategiesPlugin(self.0.clone()))]
    }
}

fn descriptor(id: &str) -> ChunkStrategyDescriptor {
    ChunkStrategyDescriptor {
        id: id.into(),
        version: "1".into(),
    }
}
