//! Portable Agent Skills contracts.
//!
//! A provider lists lightweight metadata first, then loads a skill's markdown
//! and resources only when an Agent asks for them. Implementations may use the
//! local filesystem, a database, or a remote index; the framework never assumes
//! a particular skill store.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{AgentError, RunContext};

/// Lightweight metadata exposed to a model before a skill is activated.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SkillSummary {
    /// Stable Agent Skills name.
    pub name: String,
    /// Tells the model when this skill should be used.
    pub description: String,
    /// Provider-defined source label, for example `system`, `local`, or `rag`.
    pub source: String,
    /// Provider-defined location that lets tools or system commands address the
    /// skill's files. Providers without a filesystem location can use an opaque
    /// URI.
    pub location: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<String>,
}

/// A non-fatal discovery problem. Providers omit the affected skill while
/// retaining this record for upper-application diagnostics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SkillDiagnostic {
    pub location: String,
    pub message: String,
}

/// A discovered skill catalog. Metadata is intentionally separate from loaded
/// instructions to support standard progressive disclosure.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SkillCatalog {
    pub skills: Vec<SkillSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<SkillDiagnostic>,
}

/// One loadable file below a skill's root. Its contents are not included.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SkillFile {
    pub path: String,
    pub size_bytes: u64,
}

/// Full `SKILL.md` content and the resource names available for later reads.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SkillDetail {
    pub skill: SkillSummary,
    pub skill_markdown: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<SkillFile>,
}

/// One text resource loaded after its skill was activated.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SkillResource {
    pub skill_name: String,
    pub path: String,
    pub content: String,
}

/// Supplies Agent Skills using progressive disclosure.
///
/// The same run context is passed to every method. `RunContext::run_id` remains
/// stable across a durable resume, so providers should scope one stable catalog
/// to that ID and release it after the logical run finishes or is cancelled.
#[async_trait]
pub trait SkillProvider: Send + Sync {
    /// Discover lightweight metadata without loading every `SKILL.md` body or
    /// resource file.
    async fn catalog(&self, context: &RunContext) -> Result<SkillCatalog, AgentError>;

    /// Load one activated skill's complete `SKILL.md` body and resource index.
    async fn load(&self, context: &RunContext, name: &str) -> Result<SkillDetail, AgentError>;

    /// Load one text resource listed by [`Self::load`]. Implementations must
    /// return the requested skill name and path unchanged.
    async fn read_resource(
        &self,
        context: &RunContext,
        name: &str,
        path: &str,
    ) -> Result<SkillResource, AgentError>;

    /// Drop logical-run state after the run finishes or is cancelled.
    fn release(&self, _context: &RunContext) {}
}
