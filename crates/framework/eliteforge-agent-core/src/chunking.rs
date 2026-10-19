//! Provider-neutral document chunking contracts. Hosts supply normalized blocks.
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChunkBlockKind {
    Heading,
    Paragraph,
    List,
    Table,
    Image,
    Code,
}

/// Unicode character offsets within the original block, end exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkTextRange {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChunkTable {
    pub header: ChunkTextRange,
    pub rows: Vec<ChunkTextRange>,
    pub footer: Option<ChunkTextRange>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChunkBlock {
    pub id: String,
    pub text: String,
    pub kind: ChunkBlockKind,
    pub heading_path: Vec<String>,
    /// A logical source boundary such as a worksheet. Page breaks are metadata,
    /// not mandatory semantic boundaries.
    pub boundary: Option<String>,
    pub table: Option<ChunkTable>,
    #[serde(default)]
    pub location: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChunkDocument {
    pub id: String,
    pub title: String,
    pub blocks: Vec<ChunkBlock>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChunkingOptions {
    pub max_chars: usize,
    pub overlap_chars: usize,
    pub max_chunks: usize,
    pub max_context_chars: usize,
}
impl Default for ChunkingOptions {
    fn default() -> Self {
        Self {
            max_chars: 1200,
            overlap_chars: 120,
            max_chunks: 2000,
            max_context_chars: 4800,
        }
    }
}
impl ChunkingOptions {
    pub fn validate(&self) -> Result<(), ChunkingError> {
        if self.max_chars < 32
            || self.max_chars > 32000
            || self.overlap_chars > self.max_chars / 4
            || self.max_chunks == 0
            || self.max_chunks > 10000
            || self.max_context_chars < self.max_chars
            || self.max_context_chars > 128000
        {
            return Err(ChunkingError("invalid chunking bounds".into()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChunkSourceSpan {
    pub block_id: String,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentChunk {
    pub id: String,
    pub text: String,
    pub index_text: String,
    pub heading_path: Vec<String>,
    pub spans: Vec<ChunkSourceSpan>,
    pub context_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChunkContext {
    pub id: String,
    pub text: String,
    pub spans: Vec<ChunkSourceSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChunkStrategyDescriptor {
    pub id: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChunkPlan {
    pub strategy: ChunkStrategyDescriptor,
    pub chunks: Vec<DocumentChunk>,
    pub contexts: Vec<ChunkContext>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct ChunkingError(pub String);

pub trait ChunkStrategy: Send + Sync {
    fn descriptor(&self) -> ChunkStrategyDescriptor;
    fn split(
        &self,
        document: &ChunkDocument,
        options: &ChunkingOptions,
    ) -> Result<ChunkPlan, ChunkingError>;
}

#[derive(Clone, Default)]
pub struct ChunkStrategies(BTreeMap<String, Arc<dyn ChunkStrategy>>);
impl ChunkStrategies {
    pub fn register(&mut self, strategy: Arc<dyn ChunkStrategy>) -> Result<(), ChunkingError> {
        let descriptor = strategy.descriptor();
        if descriptor.id.is_empty()
            || descriptor.version.is_empty()
            || self.0.contains_key(&descriptor.id)
        {
            return Err(ChunkingError(
                "empty or duplicate chunk strategy identity".into(),
            ));
        }
        self.0.insert(descriptor.id, strategy);
        Ok(())
    }
    pub fn descriptors(&self) -> Vec<ChunkStrategyDescriptor> {
        self.0.values().map(|s| s.descriptor()).collect()
    }
    pub fn split(
        &self,
        id: &str,
        document: &ChunkDocument,
        options: &ChunkingOptions,
    ) -> Result<ChunkPlan, ChunkingError> {
        options.validate()?;
        if document.blocks.len() > 10000
            || document.title.len() > 4096
            || document
                .blocks
                .iter()
                .map(|block| block.text.len())
                .sum::<usize>()
                > 16 * 1024 * 1024
            || document.blocks.iter().any(|block| block.id.is_empty())
        {
            return Err(ChunkingError(
                "normalized document exceeds input bounds".into(),
            ));
        }
        let strategy = self
            .0
            .get(id)
            .ok_or_else(|| ChunkingError(format!("unknown chunk strategy: {id}")))?;
        let plan = strategy.split(document, options)?;
        if plan.strategy != strategy.descriptor() || plan.chunks.len() > options.max_chunks {
            return Err(ChunkingError(
                "chunk strategy returned an invalid plan".into(),
            ));
        }
        let blocks: BTreeMap<_, _> = document
            .blocks
            .iter()
            .map(|b| (b.id.as_str(), b.text.chars().count()))
            .collect();
        if blocks.len() != document.blocks.len() {
            return Err(ChunkingError("duplicate source block identity".into()));
        }
        let contexts: std::collections::BTreeSet<_> =
            plan.contexts.iter().map(|c| c.id.as_str()).collect();
        let chunks: std::collections::BTreeSet<_> =
            plan.chunks.iter().map(|c| c.id.as_str()).collect();
        if contexts.len() != plan.contexts.len()
            || chunks.len() != plan.chunks.len()
            || plan.contexts.len() > plan.chunks.len()
            || plan.contexts.iter().any(|c| {
                c.id.is_empty()
                    || c.text.trim().is_empty()
                    || c.text.chars().count() > options.max_context_chars
                    || c.spans.is_empty()
            })
            || plan.chunks.iter().any(|c| {
                c.id.is_empty()
                    || c.text.trim().is_empty()
                    || c.index_text.trim().is_empty()
                    || c.text.chars().count() > options.max_chars
                    || c.spans.is_empty()
                    || !contexts.contains(c.context_id.as_str())
            })
            || plan
                .chunks
                .iter()
                .flat_map(|c| &c.spans)
                .chain(plan.contexts.iter().flat_map(|c| &c.spans))
                .any(|s| {
                    s.start >= s.end
                        || blocks
                            .get(s.block_id.as_str())
                            .is_none_or(|size| s.end > *size)
                })
        {
            return Err(ChunkingError(
                "chunk plan has invalid source spans or context identities".into(),
            ));
        }
        Ok(plan)
    }
}

pub struct ChunkStrategiesPlugin(pub ChunkStrategies);
#[async_trait]
impl crate::Plugin for ChunkStrategiesPlugin {
    fn id(&self) -> &str {
        "document.chunk-strategies"
    }
    async fn install(
        &self,
        context: &mut crate::PluginContext<'_>,
    ) -> Result<(), crate::PluginError> {
        context.provide(self.0.clone())
    }
}
