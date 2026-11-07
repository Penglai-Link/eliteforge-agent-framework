//! Provider-neutral, ordered message content. Adapters reject unsupported blocks.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    Json { value: Value },
    Media { media: MediaContent },
    Resource { resource: ResourceReference },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MediaContent {
    pub mime_type: String,
    pub source: MediaSource,
    pub name: Option<String>,
    /// Host/tokenizer estimate of the provider's media token charge. Required
    /// for budgeted invocation: URL length is not a media token estimate.
    pub estimated_tokens: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum MediaSource {
    Url(String),
    Base64(String),
}

/// A reference is data, never an instruction to read local files or fetch a URL.
/// Hosts/plugins resolve it explicitly before a model requiring inline content.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceReference {
    pub uri: String,
    pub mime_type: Option<String>,
    pub name: Option<String>,
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(transparent)]
pub struct Content(pub Vec<ContentBlock>);

impl Content {
    pub fn text(&self) -> String {
        self.0
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn has_media(&self) -> bool {
        self.0
            .iter()
            .any(|block| matches!(block, ContentBlock::Media { .. }))
    }

    /// Conservative text/schema estimate. Missing media estimates fail closed.
    pub fn estimated_tokens(&self) -> usize {
        self.0.iter().fold(0usize, |sum, block| {
            sum.saturating_add(match block {
                ContentBlock::Text { text } => text.len(),
                ContentBlock::Json { value } => value.to_string().len(),
                ContentBlock::Media { media } => media.estimated_tokens.unwrap_or(usize::MAX),
                ContentBlock::Resource { resource } => {
                    serde_json::to_vec(resource).map_or(usize::MAX, |v| v.len())
                }
            })
        })
    }

    /// JSON preserves block types when a text-only tool protocol needs encoding.
    pub fn protocol_text(&self) -> String {
        if self
            .0
            .iter()
            .all(|block| matches!(block, ContentBlock::Text { .. }))
        {
            self.text()
        } else {
            serde_json::to_string(self).expect("content is serializable")
        }
    }
}

impl From<String> for Content {
    fn from(text: String) -> Self {
        if text.is_empty() {
            Self::default()
        } else {
            Self(vec![ContentBlock::Text { text }])
        }
    }
}
impl From<&str> for Content {
    fn from(text: &str) -> Self {
        text.to_owned().into()
    }
}
impl From<&String> for Content {
    fn from(text: &String) -> Self {
        text.clone().into()
    }
}
impl From<&Content> for Content {
    fn from(content: &Content) -> Self {
        content.clone()
    }
}
impl From<Vec<ContentBlock>> for Content {
    fn from(blocks: Vec<ContentBlock>) -> Self {
        Self(blocks)
    }
}
impl std::fmt::Display for Content {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.protocol_text())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GenerationOptions {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub max_output_tokens: Option<u32>,
    pub stop_sequences: Option<Vec<String>>,
    pub output_schema: Option<OutputSchema>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OutputSchema {
    pub name: String,
    pub schema: Value,
}

impl GenerationOptions {
    pub fn validate(&self) -> Result<(), crate::AgentError> {
        if self.temperature.is_some_and(|v| !v.is_finite() || v < 0.0)
            || self
                .top_p
                .is_some_and(|v| !v.is_finite() || !(0.0..=1.0).contains(&v))
            || self.max_output_tokens == Some(0)
        {
            return Err(crate::AgentError::InvalidConfiguration(
                "invalid generation options".into(),
            ));
        }
        if let Some(output) = &self.output_schema {
            if output.name.is_empty()
                || !output
                    .name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                return Err(crate::AgentError::InvalidConfiguration(
                    "invalid output schema name".into(),
                ));
            }
            compile_schema(&output.schema)?;
        }
        Ok(())
    }
}

/// Remote/file schema resolution is disabled at the dependency level. A schema
/// supplied by a tool cannot cause filesystem/network access while compiling.
pub fn compile_schema(schema: &Value) -> Result<jsonschema::Validator, crate::AgentError> {
    jsonschema::validator_for(schema).map_err(|_| {
        crate::AgentError::InvalidConfiguration("invalid or unresolved JSON Schema".into())
    })
}

/// The browser and durable interaction validator use the same response contract:
/// draft 2020-12 by default, explicit draft 7/2019-09 supported, format assertions
/// enabled, no coercion/default injection, and no remote/file reference loading.
pub fn compile_response_schema(schema: &Value) -> Result<jsonschema::Validator, crate::AgentError> {
    if let Some(dialect) = schema.get("$schema") {
        if !matches!(dialect.as_str(), Some(
            "https://json-schema.org/draft/2020-12/schema"
            | "https://json-schema.org/draft/2019-09/schema"
            | "http://json-schema.org/draft-07/schema#"
            | "http://json-schema.org/draft-07/schema"
            | "https://json-schema.org/draft-07/schema#"
            | "https://json-schema.org/draft-07/schema"
        )) {
            return Err(crate::AgentError::InvalidConfiguration("unsupported response schema dialect".into()));
        }
    }
    jsonschema::options().should_validate_formats(true).build(schema).map_err(|error| {
        crate::AgentError::InvalidConfiguration(format!("invalid or unresolved response JSON Schema: {error}"))
    })
}
