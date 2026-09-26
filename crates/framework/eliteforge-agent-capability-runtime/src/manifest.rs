use crate::{RuntimeError, RuntimeResult};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const CAPABILITY_PROTOCOL_VERSION: u32 = 1;
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionCategory {
    Execution,
    Model,
    Tools,
    Context,
    Storage,
    Middleware,
    Observation,
    Custom,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Multiplicity {
    Single,
    Multiple,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionContract {
    pub id: String,
    pub owner: String,
    pub version: semver::Version,
    pub category: ExtensionCategory,
    pub multiplicity: Multiplicity,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionImplementation {
    pub id: String,
    pub extension: String,
    pub contract: semver::VersionReq,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendEntrypoint {
    /// Platform triple to package-relative executable path.
    pub executables: BTreeMap<String, String>,
    #[serde(default)]
    pub args: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageDependency {
    pub id: String,
    pub version: semver::VersionReq,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageDescriptor {
    pub id: String,
    pub title: String,
    pub plugin: String,
    pub entry: String,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub topics: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpDeclaration {
    pub id: String,
    #[serde(flatten)]
    pub transport: McpDeclarationTransport,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case", deny_unknown_fields)]
pub enum McpDeclarationTransport {
    Stdio {
        program: String,
        #[serde(default)]
        args: Vec<String>,
    },
    Http {
        url: String,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityPackManifest {
    pub manifest_version: u32,
    pub id: String,
    pub name: String,
    pub version: semver::Version,
    pub framework: semver::VersionReq,
    #[serde(default)]
    pub dependencies: Vec<PackageDependency>,
    #[serde(default)]
    pub backend: Option<BackendEntrypoint>,
    #[serde(default)]
    pub plugins: Vec<String>,
    #[serde(default)]
    pub extension_points: Vec<ExtensionContract>,
    #[serde(default)]
    pub implementations: Vec<ExtensionImplementation>,
    #[serde(default)]
    pub pages: Vec<PageDescriptor>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub mcp: Vec<McpDeclaration>,
    #[serde(default)]
    pub resources: Vec<String>,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub config_schema: serde_json::Value,
}
pub fn validate_id(id: &str) -> RuntimeResult<()> {
    if id.is_empty()
        || id.starts_with('.')
        || id.len() > 128
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".-_".contains(&c))
    {
        return Err(RuntimeError::Invalid(format!(
            "invalid capability identity: {id}"
        )));
    }
    Ok(())
}
pub fn safe_relative(path: &str) -> RuntimeResult<std::path::PathBuf> {
    let path = std::path::Path::new(path);
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        || path.to_string_lossy().contains('\\')
        || path.to_string_lossy().contains(':')
        || path.to_string_lossy().contains('\0')
        || path.components().any(|component| {
            let name = component.as_os_str().to_string_lossy();
            let stem = name
                .split('.')
                .next()
                .unwrap_or_default()
                .to_ascii_uppercase();
            name.ends_with(['.', ' '])
                || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                || (stem.is_ascii()
                    && stem.len() == 4
                    && matches!(&stem[..3], "COM" | "LPT")
                    && matches!(stem.as_bytes()[3], b'1'..=b'9'))
        })
    {
        return Err(RuntimeError::Invalid(
            "resource must be a safe relative path".into(),
        ));
    }
    Ok(path.to_path_buf())
}
impl CapabilityPackManifest {
    pub fn validate(&self) -> RuntimeResult<()> {
        validate_id(&self.id)?;
        if self.manifest_version != 1
            || !self
                .framework
                .matches(&semver::Version::parse(env!("CARGO_PKG_VERSION")).expect("crate version"))
        {
            return Err(RuntimeError::Invalid(
                "unsupported manifest or framework version".into(),
            ));
        }
        let mut ids = std::collections::HashSet::new();
        for id in self
            .plugins
            .iter()
            .chain(self.implementations.iter().map(|v| &v.id))
            .chain(self.pages.iter().map(|v| &v.id))
            .chain(self.mcp.iter().map(|v| &v.id))
        {
            validate_id(id)?;
            if !ids.insert(id) {
                return Err(RuntimeError::Invalid(format!(
                    "duplicate contribution: {id}"
                )));
            }
        }
        let mut contracts = std::collections::HashSet::new();
        for contract in &self.extension_points {
            validate_id(&contract.id)?;
            if (contract.owner != self.id && !self.plugins.contains(&contract.owner))
                || contract.id.starts_with("framework.")
                || !contracts.insert(&contract.id)
            {
                return Err(RuntimeError::Invalid(
                    "extension contracts must belong to their defining capability package".into(),
                ));
            }
        }
        for implementation in &self.implementations {
            validate_id(&implementation.extension)?;
            for dependency in &implementation.dependencies {
                validate_id(dependency)?;
            }
        }
        for mcp in &self.mcp {
            match &mcp.transport {
                McpDeclarationTransport::Stdio { program, .. } => {
                    safe_relative(program)?;
                }
                McpDeclarationTransport::Http { url } => {
                    let url = reqwest::Url::parse(url)
                        .map_err(|e| RuntimeError::Invalid(e.to_string()))?;
                    if !matches!(url.scheme(), "http" | "https") {
                        return Err(RuntimeError::Invalid("unsupported MCP URL".into()));
                    }
                }
            }
        }
        let mut dependencies = std::collections::HashSet::new();
        for dependency in &self.dependencies {
            validate_id(&dependency.id)?;
            if dependency.id == self.id || !dependencies.insert(&dependency.id) {
                return Err(RuntimeError::Invalid(
                    "self or duplicate package dependency".into(),
                ));
            }
        }
        if self
            .backend
            .as_ref()
            .is_some_and(|backend| backend.executables.is_empty())
        {
            return Err(RuntimeError::Invalid(
                "backend must declare a platform executable".into(),
            ));
        }
        for path in self
            .skills
            .iter()
            .chain(&self.resources)
            .chain(self.pages.iter().map(|p| &p.entry))
            .chain(self.backend.iter().flat_map(|b| b.executables.values()))
        {
            safe_relative(path)?;
        }
        if !self.implementations.is_empty() && self.backend.is_none() {
            return Err(RuntimeError::Invalid(
                "external implementations require a backend".into(),
            ));
        }
        Ok(())
    }
}
/// Stable built-in protocol contracts, independent of Rust TypeId and source paths.
pub fn framework_extension_contracts() -> Vec<ExtensionContract> {
    use ExtensionCategory::*;
    let groups = [
        (Execution, vec!["agent-loop", "workflow-node"]),
        (
            Model,
            vec![
                "chat-model",
                "model-discovery",
                "model-selection",
                "token-estimator",
                "embedding",
                "rerank",
                "image",
                "video",
                "speech-recognition",
                "speech-synthesis",
                "realtime",
            ],
        ),
        (Tools, vec!["tool-provider", "tool-policy"]),
        (
            Context,
            vec![
                "context-provider",
                "context-contributor",
                "context-assembler",
                "context-budget",
                "context-reducer",
                "skills",
                "long-term-memory",
            ],
        ),
        (
            Storage,
            vec!["conversation-store", "state-store", "interaction-store"],
        ),
        (
            Middleware,
            vec![
                "run-middleware",
                "model-middleware",
                "tool-middleware",
                "node-middleware",
                "lifecycle-hook",
            ],
        ),
        (Observation, vec!["event-sink", "run-output"]),
    ];
    let mut contracts: Vec<_> = groups
        .into_iter()
        .flat_map(|(category, ids)| {
            ids.into_iter().map(move |id| ExtensionContract {
                id: format!("framework.{id}"),
                owner: "framework".into(),
                version: semver::Version::new(1, 0, 0),
                category: category.clone(),
                multiplicity: if matches!(
                    id,
                    "agent-loop"
                        | "model-selection"
                        | "context-assembler"
                        | "context-budget"
                        | "context-reducer"
                        | "conversation-store"
                        | "state-store"
                        | "interaction-store"
                        | "long-term-memory"
                ) {
                    Multiplicity::Single
                } else {
                    Multiplicity::Multiple
                },
            })
        })
        .collect();
    for id in ["context.summary-store", "context.artifact-store"] {
        contracts.push(ExtensionContract {
            id: id.into(),
            owner: "context".into(),
            version: semver::Version::new(1, 0, 0),
            category: Storage,
            multiplicity: Multiplicity::Single,
        });
    }
    contracts
}
