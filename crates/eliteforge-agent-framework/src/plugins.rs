use crate::*;
use async_trait::async_trait;
use std::sync::Arc;
pub struct ReactPlugin;

#[async_trait]
impl Plugin for ReactPlugin {
    fn id(&self) -> &str {
        "builtin.react-loop"
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        context.provide(LoopService(Arc::new(crate::react::ReactAgentLoop)))
    }
}

pub(crate) struct DefaultsPlugin {
    pub after: Vec<String>,
    pub default_models: bool,
}

#[async_trait]
impl Plugin for DefaultsPlugin {
    fn id(&self) -> &str {
        "builtin.framework-defaults"
    }
    fn dependencies(&self) -> Vec<String> {
        self.after.clone()
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        context.contribute(WorkflowNodeService(Arc::new(ReactWorkflowNode)))?;
        if context.services().get::<LoopService>().is_none() {
            context.provide(LoopService(Arc::new(crate::react::ReactAgentLoop)))?;
        }
        #[cfg(feature = "genai")]
        if self.default_models
            && context
                .services()
                .all::<ModelConnectionService>()
                .is_empty()
            && context
                .services()
                .get::<LoopService>()
                .is_some_and(|s| s.0.requires_model())
        {
            eliteforge_agent_official_plugin_model_genai::GenaiPlugin::default()
                .install(context)
                .await?;
        }
        #[cfg(not(feature = "genai"))]
        let _ = self.default_models;
        if context.services().all::<ToolProviderService>().is_empty() {
            context.contribute(ToolProviderService(Arc::new(StaticToolProvider::default())))?;
        }
        if context.services().all::<ContextService>().is_empty() {
            context.contribute(ContextService(Arc::new(FixedContext::default())))?;
        }
        if context.services().all::<EventService>().is_empty() {
            context.contribute(EventService(Arc::new(NoopEvents)))?;
        }
        Ok(())
    }
}
