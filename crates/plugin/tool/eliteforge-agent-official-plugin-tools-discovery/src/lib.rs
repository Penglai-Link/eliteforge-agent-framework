//! Installs the stable gateway: `tool_search` discovers the wrapped inventory
//! as compact summaries; `tool_schema` returns one exact execution schema;
//! `tool_execute` addresses any wrapped tool as arguments, and the execution
//! loop unwraps envelopes at admission so wrapped tools keep their own
//! schemas, policies, middleware and executors.
//!
//! With no explicit providers the plugin wraps the host's whole tool catalog
//! (the execution loop re-mints the search binding with the complete leased
//! inventory). With explicit providers the plugin additionally leases those
//! providers' bindings as wrapped inventory. Provider `lease` still runs at
//! run setup; schemas are discovered on demand. The model-visible tool list
//! stays stable for the whole run, which preserves provider prompt caches.
use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use eliteforge_agent_core::*;

/// Wrap providers instead of installing the same providers a second time.
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
    fn descriptors(&self) -> Option<Vec<ToolDescriptor>> {
        Some(vec![
            search_descriptor(),
            schema_descriptor(),
            execute_descriptor(),
        ])
    }

    fn catalog_metadata(&self) -> Option<Vec<ToolCatalogEntry>> {
        Some(
            vec![
                search_descriptor(),
                schema_descriptor(),
                execute_descriptor(),
            ]
            .into_iter()
            .map(|descriptor| ToolCatalogEntry {
                descriptor,
                domain_code: Some("tool".into()),
                category: Some(ToolCategory::localized(
                    "execution",
                    "执行工具",
                    "Execution tools",
                )),
            })
            .collect(),
        )
    }

    async fn lease(&self, context: &RunContext) -> Result<ToolLease, AgentError> {
        let mut tools = Vec::new();
        let mut ids = BTreeSet::from([
            TOOL_SEARCH_ID.to_owned(),
            TOOL_SCHEMA_ID.to_owned(),
            TOOL_EXECUTE_ID.to_owned(),
        ]);
        let mut names = BTreeSet::from([
            TOOL_SEARCH_CALL_NAME.to_owned(),
            TOOL_SCHEMA_CALL_NAME.to_owned(),
            TOOL_EXECUTE_CALL_NAME.to_owned(),
        ]);
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
            for binding in lease.tools {
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
                tools.push(binding);
            }
        }
        // Contribute the gateway trio. The execution loop re-mints discovery
        // with the complete leased inventory, so these fallback executors only
        // cover hosts that dispatch them without the loop's help.
        let entries: Vec<GatewayInventoryEntry> = tools
            .iter()
            .filter(|binding| binding.exposed)
            .map(|binding| {
                let entry = GatewayInventoryEntry::new(
                    eliteforge_agent_core::batch_tool_descriptor(&binding.descriptor),
                )
                .with_summary_description(binding.descriptor.description.clone());
                match &binding.domain_code {
                    Some(code) => entry.with_domain_code(code.clone()),
                    None => entry,
                }
            })
            .collect();
        tools.push(
            ToolBinding::new(
                search_descriptor(),
                GatewaySearch {
                    entries: entries.clone(),
                },
            )
            .with_domain_code("tool")
            .with_category(ToolCategory::localized(
                "execution",
                "执行工具",
                "Execution tools",
            )),
        );
        tools.push(
            ToolBinding::new(schema_descriptor(), GatewaySchema { entries })
                .with_domain_code("tool")
                .with_category(ToolCategory::localized(
                    "execution",
                    "执行工具",
                    "Execution tools",
                )),
        );
        tools.push(
            ToolBinding::new(execute_descriptor(), GatewayExecute)
                .with_domain_code("tool")
                .with_category(ToolCategory::localized(
                    "execution",
                    "执行工具",
                    "Execution tools",
                )),
        );
        Ok(ToolLease::new(tools))
    }
}
