use std::sync::Arc;

use async_trait::async_trait;
use eliteforge_agent_plugin_core::{Plugin, PluginContext, PluginError};

use crate::*;

pub struct ConversationStoreService(pub Arc<dyn ConversationStore>);
pub struct LongTermMemoryService(pub Arc<dyn LongTermMemory>);
pub struct LoopService(pub Arc<dyn AgentLoop>);
pub struct ExternalAgentService(pub Arc<dyn ExternalAgent>);
pub struct ModelCatalogStoreService(pub Arc<dyn ModelCatalogStore>);
pub struct ToolProviderService(pub Arc<dyn ToolProvider>);
pub struct EventService(pub Arc<dyn EventSink>);
pub struct ContextReducerService(pub Arc<dyn ContextReducer>);
pub struct ContextAssemblerService(pub Arc<dyn ContextAssembler>);
pub struct ContextBudgetPolicyService(pub Arc<dyn ContextBudgetPolicy>);
pub struct ContextLayoutService(pub ContextLayout);
pub struct ContextLayoutPlugin(pub ContextLayout);
#[async_trait]
impl Plugin for ContextLayoutPlugin {
    fn id(&self) -> &str {
        "framework.context-layout"
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        context.provide(ContextLayoutService(self.0.clone()))
    }
}

macro_rules! unique_plugin {
    ($name:ident, $interface:ident, $service:ident, $id:literal) => {
        pub struct $name {
            implementation: Arc<dyn $interface>,
        }
        impl $name {
            pub fn new(implementation: impl $interface + 'static) -> Self {
                Self {
                    implementation: Arc::new(implementation),
                }
            }
            pub fn from_arc(implementation: Arc<dyn $interface>) -> Self {
                Self { implementation }
            }
        }
        #[async_trait]
        impl Plugin for $name {
            fn id(&self) -> &str {
                $id
            }
            async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
                context.provide($service(self.implementation.clone()))?;
                context.contribute_extension(self.implementation.clone())
            }
        }
    };
}

macro_rules! selectable_plugin {
    ($name:ident, $interface:ident, $service:ident, $default:literal) => {
        pub struct $name {
            id: String,
            implementation: Arc<dyn $interface>,
        }
        impl $name {
            pub fn new(implementation: impl $interface + 'static) -> Self {
                Self::from_arc(Arc::new(implementation))
            }
            pub fn from_arc(implementation: Arc<dyn $interface>) -> Self {
                Self {
                    id: $default.into(),
                    implementation,
                }
            }
            pub fn named(id: impl Into<String>, implementation: impl $interface + 'static) -> Self {
                Self {
                    id: id.into(),
                    implementation: Arc::new(implementation),
                }
            }
            pub fn named_arc(id: impl Into<String>, implementation: Arc<dyn $interface>) -> Self {
                Self {
                    id: id.into(),
                    implementation,
                }
            }
        }
        #[async_trait]
        impl Plugin for $name {
            fn id(&self) -> &str {
                &self.id
            }
            async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
                context.contribute($service(self.implementation.clone()))?;
                context.contribute_extension(self.implementation.clone())
            }
        }
    };
}

pub struct ChatModelPlugin(ModelConnectionPlugin);
impl ChatModelPlugin {
    pub fn new(model: impl ChatModel + 'static) -> Self {
        Self::from_arc(Arc::new(model))
    }
    pub fn from_arc(model: Arc<dyn ChatModel>) -> Self {
        Self(ModelConnectionPlugin::new(ModelConnection::new(
            "default",
            model,
            vec![ModelDescriptor::new("default")],
        )))
    }
}
#[async_trait]
impl Plugin for ChatModelPlugin {
    fn id(&self) -> &str {
        self.0.id()
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        self.0.install(context).await
    }
}
unique_plugin!(
    ConversationStorePlugin,
    ConversationStore,
    ConversationStoreService,
    "framework.conversations"
);
selectable_plugin!(AgentLoopPlugin, AgentLoop, LoopService, "framework.loop");
selectable_plugin!(
    ExternalAgentPlugin,
    ExternalAgent,
    ExternalAgentService,
    "framework.external-agent"
);
unique_plugin!(
    ModelCatalogStorePlugin,
    ModelCatalogStore,
    ModelCatalogStoreService,
    "framework.model-catalog-store"
);
unique_plugin!(
    LongTermMemoryPlugin,
    LongTermMemory,
    LongTermMemoryService,
    "framework.long-term-memory"
);
selectable_plugin!(
    ContextAssemblerPlugin,
    ContextAssembler,
    ContextAssemblerService,
    "framework.context-assembler"
);
selectable_plugin!(
    ContextBudgetPolicyPlugin,
    ContextBudgetPolicy,
    ContextBudgetPolicyService,
    "framework.context-budget"
);
selectable_plugin!(
    ContextReducerPlugin,
    ContextReducer,
    ContextReducerService,
    "framework.context-reducer"
);

macro_rules! contribution_plugin {
    ($name:ident, $interface:ident, $service:ident) => {
        pub struct $name {
            id: String,
            implementation: Arc<dyn $interface>,
        }
        impl $name {
            pub fn new(id: impl Into<String>, implementation: impl $interface + 'static) -> Self {
                Self {
                    id: id.into(),
                    implementation: Arc::new(implementation),
                }
            }
            pub fn from_arc(id: impl Into<String>, implementation: Arc<dyn $interface>) -> Self {
                Self {
                    id: id.into(),
                    implementation,
                }
            }
        }
        #[async_trait]
        impl Plugin for $name {
            fn id(&self) -> &str {
                &self.id
            }
            async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
                context.contribute($service(self.implementation.clone()))?;
                context.contribute_extension(self.implementation.clone())
            }
        }
    };
}

contribution_plugin!(ToolProviderPlugin, ToolProvider, ToolProviderService);
contribution_plugin!(EventPlugin, EventSink, EventService);
contribution_plugin!(RunOutputPlugin, RunOutputFactory, RunOutputService);

pub type EventSinkPlugin = EventPlugin;

#[derive(Clone, Default)]
pub struct StaticToolProvider {
    tools: Vec<ToolBinding>,
}

impl StaticToolProvider {
    pub fn new(tools: Vec<ToolBinding>) -> Self {
        Self { tools }
    }
}

#[async_trait]
impl ToolProvider for StaticToolProvider {
    fn descriptors(&self) -> Option<Vec<ToolDescriptor>> {
        Some(
            self.tools
                .iter()
                .map(|tool| tool.descriptor.clone())
                .collect(),
        )
    }
    fn catalog_metadata(&self) -> Option<Vec<crate::ToolCatalogEntry>> {
        Some(self.tools.iter().map(ToolBinding::catalog_entry).collect())
    }
    async fn lease(&self, _context: &RunContext) -> Result<ToolLease, AgentError> {
        Ok(ToolLease::new(self.tools.clone()))
    }
}

/// Registers one explicitly described tool through the normal tool-provider
/// path. Use this from `#[capability_factory]` when a single `Tool` needs a
/// descriptor, visibility, or parallelism configuration.
pub struct ToolBindingPlugin {
    id: String,
    binding: ToolBinding,
}

impl ToolBindingPlugin {
    pub fn new(id: impl Into<String>, binding: ToolBinding) -> Self {
        Self {
            id: id.into(),
            binding,
        }
    }
}

#[async_trait]
impl Plugin for ToolBindingPlugin {
    fn id(&self) -> &str {
        &self.id
    }

    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let provider: Arc<dyn ToolProvider> =
            Arc::new(StaticToolProvider::new(vec![self.binding.clone()]));
        context.contribute(ToolProviderService(provider.clone()))?;
        context.contribute_extension(provider)
    }
}

pub struct NoopEvents;

#[async_trait]
impl EventSink for NoopEvents {
    async fn on_event(&self, _context: &RunContext, _event: &AgentEvent) {}
}

pub struct RunMiddlewareService(pub Arc<dyn RunMiddleware>);
pub struct RunLifecycleHookService(pub Arc<dyn RunLifecycleHook>);
contribution_plugin!(
    RunLifecycleHookPlugin,
    RunLifecycleHook,
    RunLifecycleHookService
);
pub struct ContextContributorService(pub Arc<dyn ContextContributor>);
contribution_plugin!(
    ModelMiddlewarePlugin,
    ModelMiddleware,
    ModelMiddlewareService
);
contribution_plugin!(ToolMiddlewarePlugin, ToolMiddleware, ToolMiddlewareService);
contribution_plugin!(RunMiddlewarePlugin, RunMiddleware, RunMiddlewareService);
pub struct ContextContributorPlugin {
    id: String,
    implementation: Arc<dyn ContextContributor>,
}
impl ContextContributorPlugin {
    pub fn new(id: impl Into<String>, implementation: impl ContextContributor + 'static) -> Self {
        Self::from_arc(id, Arc::new(implementation))
    }
    pub fn from_arc(id: impl Into<String>, implementation: Arc<dyn ContextContributor>) -> Self {
        Self {
            id: id.into(),
            implementation,
        }
    }
}
struct RegisteredInstructionSource {
    id: String,
    implementation: Arc<dyn ContextContributor>,
}
#[async_trait]
impl ContextContributor for RegisteredInstructionSource {
    fn kind(&self) -> ContextSourceKind {
        ContextSourceKind::Instruction
    }
    fn block_id(&self) -> &str {
        &self.id
    }
    fn layout_metadata(&self) -> ContextBlockMetadata {
        self.implementation.layout_metadata()
    }
    fn default_block_config(&self) -> Option<ContextBlockConfig> {
        self.implementation.default_block_config().map(|mut config| {
            config.id.clone_from(&self.id);
            config
        })
    }
    async fn contribute(&self, input: &ContextInput<'_>) -> Result<ContextBlock, AgentError> {
        let mut block = self.implementation.contribute(input).await?;
        if block.id != self.implementation.block_id()
            || !matches!(block.trust, ContextTrust::Instruction | ContextTrust::Mixed)
        {
            return Err(AgentError::InvalidConfiguration(
                "instruction source changed its identity or trust".into(),
            ));
        }
        block.id.clone_from(&self.id);
        Ok(block)
    }
}
#[async_trait]
impl Plugin for ContextContributorPlugin {
    fn id(&self) -> &str {
        &self.id
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        if self.implementation.kind() == ContextSourceKind::History
            || [INSTRUCTIONS_BLOCK, HISTORY_BLOCK].contains(&self.id.as_str())
            || [INSTRUCTIONS_BLOCK, HISTORY_BLOCK].contains(&self.implementation.block_id())
        {
            return Err(PluginError::message(
                "builtin context sources are owned by the SDK",
            ));
        }
        let source: Arc<dyn ContextContributor> =
            if self.implementation.kind() == ContextSourceKind::Instruction {
                Arc::new(RegisteredInstructionSource {
                    id: self.id.clone(),
                    implementation: self.implementation.clone(),
                })
            } else {
                self.implementation.clone()
            };
        context.contribute(ContextContributorService(source.clone()))?;
        context.contribute_extension(source)
    }
}

pub struct ToolPolicyService(pub Arc<dyn ToolPolicy>);
contribution_plugin!(ToolPolicyPlugin, ToolPolicy, ToolPolicyService);

pub struct ModelMiddlewareService(pub Arc<dyn ModelMiddleware>);
pub struct ToolMiddlewareService(pub Arc<dyn ToolMiddleware>);
