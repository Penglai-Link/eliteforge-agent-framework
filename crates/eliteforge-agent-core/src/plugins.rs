use std::sync::Arc;

use async_trait::async_trait;
use eliteforge_agent_plugin_kernel::{Plugin, PluginContext, PluginError};

use crate::*;

pub struct ConversationStoreService(pub Arc<dyn ConversationStore>);
pub struct LongTermMemoryService(pub Arc<dyn LongTermMemory>);
pub struct LoopService(pub Arc<dyn AgentLoop>);
pub struct ToolProviderService(pub Arc<dyn ToolProvider>);
pub struct ContextService(pub Arc<dyn ContextProvider>);
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
pub struct WorkflowNodeService(pub Arc<dyn WorkflowNode>);

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
                context.provide($service(self.implementation.clone()))
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
unique_plugin!(AgentLoopPlugin, AgentLoop, LoopService, "framework.loop");
unique_plugin!(
    LongTermMemoryPlugin,
    LongTermMemory,
    LongTermMemoryService,
    "framework.long-term-memory"
);
unique_plugin!(
    ContextAssemblerPlugin,
    ContextAssembler,
    ContextAssemblerService,
    "framework.context-assembler"
);
unique_plugin!(
    ContextBudgetPolicyPlugin,
    ContextBudgetPolicy,
    ContextBudgetPolicyService,
    "framework.context-budget"
);
unique_plugin!(
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
                context.contribute($service(self.implementation.clone()))
            }
        }
    };
}

contribution_plugin!(ToolProviderPlugin, ToolProvider, ToolProviderService);
contribution_plugin!(ContextPlugin, ContextProvider, ContextService);
contribution_plugin!(EventPlugin, EventSink, EventService);
contribution_plugin!(WorkflowNodePlugin, WorkflowNode, WorkflowNodeService);

pub type ContextProviderPlugin = ContextPlugin;
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
    async fn lease(&self, _context: &RunContext) -> Result<ToolLease, AgentError> {
        Ok(ToolLease::new(self.tools.clone()))
    }
}

#[derive(Clone, Default)]
pub struct FixedContext {
    messages: Vec<Message>,
}

impl FixedContext {
    pub fn new(instructions: impl Into<String>) -> Self {
        Self {
            messages: vec![Message::system(instructions.into())],
        }
    }
    pub fn messages(messages: Vec<Message>) -> Self {
        Self { messages }
    }
}

#[async_trait]
impl ContextProvider for FixedContext {
    async fn context(&self, _context: &RunContext) -> Result<Vec<Message>, AgentError> {
        Ok(self.messages.clone())
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
pub struct WorkflowNodeMiddlewareService(pub Arc<dyn WorkflowNodeMiddleware>);
contribution_plugin!(
    WorkflowNodeMiddlewarePlugin,
    WorkflowNodeMiddleware,
    WorkflowNodeMiddlewareService
);
pub struct ContextContributorService(pub Arc<dyn ContextContributor>);
contribution_plugin!(
    ModelMiddlewarePlugin,
    ModelMiddleware,
    ModelMiddlewareService
);
contribution_plugin!(ToolMiddlewarePlugin, ToolMiddleware, ToolMiddlewareService);
contribution_plugin!(RunMiddlewarePlugin, RunMiddleware, RunMiddlewareService);
contribution_plugin!(
    ContextContributorPlugin,
    ContextContributor,
    ContextContributorService
);

pub struct ToolPolicyService(pub Arc<dyn ToolPolicy>);
contribution_plugin!(ToolPolicyPlugin, ToolPolicy, ToolPolicyService);

pub struct ModelMiddlewareService(pub Arc<dyn ModelMiddleware>);
pub struct ToolMiddlewareService(pub Arc<dyn ToolMiddleware>);
