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
        let agent_loop: Arc<dyn AgentLoop> = Arc::new(crate::react::ReactAgentLoop);
        context.contribute(LoopService(agent_loop.clone()))?;
        context.contribute_extension(agent_loop)
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
        if !context.services().contains::<LoopService>() {
            let agent_loop: Arc<dyn AgentLoop> = Arc::new(crate::react::ReactAgentLoop);
            context.contribute(LoopService(agent_loop.clone()))?;
            context.contribute_extension(agent_loop)?;
        }
        #[cfg(feature = "genai")]
        if self.default_models
            && context
                .services()
                .all::<ModelConnectionService>()
                .is_empty()
            && context
                .services()
                .get::<ModelRuntimeService>()
                .is_none_or(|service| service.0.snapshot().chat.models().is_empty())
            && context
                .services()
                .get::<LoopService>()
                .into_iter()
                .chain(context.services().all::<LoopService>())
                .any(|s| s.0.requires_model())
        {
            eliteforge_agent_official_plugin_model_genai::GenaiPlugin::default()
                .install(context)
                .await?;
        }
        #[cfg(not(feature = "genai"))]
        let _ = self.default_models;
        if context.services().all::<ToolProviderService>().is_empty() {
            let tool_provider: Arc<dyn ToolProvider> = Arc::new(StaticToolProvider::default());
            context.contribute(ToolProviderService(tool_provider.clone()))?;
            context.contribute_extension(tool_provider)?;
        }
        if context.services().all::<EventService>().is_empty() {
            let event_sink: Arc<dyn EventSink> = Arc::new(NoopEvents);
            context.contribute(EventService(event_sink.clone()))?;
            context.contribute_extension(event_sink)?;
        }
        Ok(())
    }
}
