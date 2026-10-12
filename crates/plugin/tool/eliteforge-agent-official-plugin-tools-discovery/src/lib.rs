//! Defers model-visible schemas while preserving the original executors, leases
//! and policy chain. Only explicitly supplied providers enter this catalog.
use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use eliteforge_agent_core::*;
use serde::Deserialize;
use serde_json::{Value, json};

/// Wrap providers instead of installing the same providers a second time.
/// Provider `lease` still runs at run setup; schemas are exposed on demand.
/// This reduces model context, not provider connection/discovery cost.
#[derive(Clone, Default)]
pub struct ToolDiscoveryPlugin {
    providers: Vec<Arc<dyn ToolProvider>>,
}

impl ToolDiscoveryPlugin {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn provider(mut self, provider: impl ToolProvider + 'static) -> Self {
        self.providers.push(Arc::new(provider));
        self
    }

    pub fn shared_provider(mut self, provider: Arc<dyn ToolProvider>) -> Self {
        self.providers.push(provider);
        self
    }
}

#[async_trait]
impl Plugin for ToolDiscoveryPlugin {
    fn id(&self) -> &str {
        "framework.tools.discovery"
    }

    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        ToolProviderPlugin::new(self.id(), self.clone())
            .install(context)
            .await
    }
}

#[async_trait]
impl ToolProvider for ToolDiscoveryPlugin {
    async fn lease(&self, context: &RunContext) -> Result<ToolLease, AgentError> {
        let mut tools = Vec::new();
        let mut descriptors = Vec::new();
        let mut ids = BTreeSet::from(["framework.tools.search".to_owned()]);
        let mut names = BTreeSet::from(["tool_search".to_owned()]);
        let mut bytes = 0usize;
        if self.providers.len() > 64 {
            return Err(AgentError::ToolSetup(
                "discovery supports at most 64 providers".into(),
            ));
        }
        for provider in &self.providers {
            context.check_cancelled()?;
            let mut lease = provider.lease(context).await?;
            context.own_resources(std::mem::take(&mut lease.resources));
            for mut binding in lease.tools {
                if !ids.insert(binding.descriptor.id.clone())
                    || !names.insert(binding.descriptor.call_name.clone())
                {
                    return Err(AgentError::ToolSetup(
                        "duplicate discovery tool ID or call name".into(),
                    ));
                }
                bytes = bytes.saturating_add(
                    serde_json::to_vec(&binding.descriptor)
                        .map_err(|e| AgentError::ToolSetup(e.to_string()))?
                        .len(),
                );
                if tools.len() >= 1024 || bytes > 2 * 1024 * 1024 {
                    return Err(AgentError::ToolSetup(
                        "discovery catalog exceeds 1024 tools or 2 MiB of schemas".into(),
                    ));
                }
                // Already-hidden bindings remain private to their provider's own
                // activation flow. The wrapper only defers originally exposed tools.
                if binding.exposed {
                    descriptors.push(binding.descriptor.clone());
                }
                binding.exposed = false;
                tools.push(binding);
            }
        }
        descriptors.sort_by(|a, b| a.id.cmp(&b.id));
        tools.push(ToolBinding::new(ToolDescriptor::new("framework.tools.search", "tool_search",
            "Find authorized tools by words in their ID, name or description. Returns full schemas and enables the matching tools for this run. Uses AND matching; choose a specific name to narrow results. Execution still requires normal tool policy checks.",
            json!({"type":"object","additionalProperties":false,"required":["query"],"properties":{
                "query":{"type":"string","minLength":1,"maxLength":512},
                "limit":{"type":"integer","minimum":1,"maximum":20}
            }})).with_operation_kind(eliteforge_agent_core::ToolOperationKind::Query), Search { descriptors }));
        Ok(ToolLease::new(tools))
    }
}

struct Search {
    descriptors: Vec<ToolDescriptor>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchInput {
    query: String,
    limit: Option<usize>,
}

#[async_trait]
impl Tool for Search {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let input: SearchInput =
            serde_json::from_value(arguments).map_err(|e| ToolError::Failed(e.to_string()))?;
        let terms: Vec<_> = input
            .query
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        let limit = input.limit.unwrap_or(5);
        if terms.is_empty() || input.query.len() > 2048 || !(1..=20).contains(&limit) {
            return Err(ToolError::Failed("invalid query or limit".into()));
        }
        let mut selected = Vec::new();
        let mut matches = 0usize;
        let mut bytes = 0usize;
        for descriptor in &self.descriptors {
            if context.is_cancelled() {
                return Err(ToolError::Failed("tool discovery cancelled".into()));
            }
            if context.is_tool_revoked(&descriptor.id) {
                continue;
            }
            let haystack = format!(
                "{} {} {}",
                descriptor.id, descriptor.call_name, descriptor.description
            )
            .to_lowercase();
            if !terms.iter().all(|term| haystack.contains(term)) {
                continue;
            }
            matches += 1;
            let size = serde_json::to_vec(descriptor)
                .map_err(|e| ToolError::Failed(e.to_string()))?
                .len();
            if selected.len() < limit && bytes.saturating_add(size) <= 64 * 1024 {
                bytes += size;
                selected.push(descriptor.clone());
            }
        }
        let ids = selected.iter().map(|d| d.id.clone()).collect();
        let data = json!({"tools":selected,"matched":matches,"truncated":matches>selected.len()});
        let mut output = ToolOutput::text(data.to_string()).expose_tools(ids);
        output.structured = Some(data);
        Ok(output)
    }
}
