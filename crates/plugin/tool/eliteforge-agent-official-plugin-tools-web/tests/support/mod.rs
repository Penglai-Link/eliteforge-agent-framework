use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_test_support::TestBuilder;
use serde_json::Value;
use std::sync::{Arc, Mutex};
struct Invoke {
    name: &'static str,
    args: Value,
    output: Arc<Mutex<Option<Result<ToolOutput, String>>>>,
    cancel: bool,
}
#[async_trait]
impl AgentLoop for Invoke {
    async fn run(
        &self,
        _: &RunRequest,
        ctx: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        for provider in &services.tools {
            let mut lease = provider.lease(ctx).await?;
            ctx.own_resources(std::mem::take(&mut lease.resources));
            if let Some(tool) = lease
                .tools
                .iter()
                .find(|t| t.descriptor.call_name == self.name)
            {
                if self.cancel {
                    ctx.cancellation.cancel();
                }
                *self.output.lock().unwrap() = Some(
                    tool.executor
                        .call(self.args.clone(), ctx)
                        .await
                        .map_err(|e| e.to_string()),
                );
                return Ok(RunResult {
                    output: String::new(),
                    content: Content::default(),
                    structured: None,
                    status: RunStatus::Completed,
                    interactions: vec![],
                    usage: Usage::default(),
                    steps: 0,
                    tool_calls: 0,
                    external_session: None,
                });
            }
        }
        Err(AgentError::Run("tool not registered".into()))
    }
}
pub async fn invoke(
    plugin: impl Plugin + 'static,
    name: &'static str,
    args: Value,
    cancel: bool,
) -> Result<ToolOutput, String> {
    let output = Arc::new(Mutex::new(None));
    let agent = TestBuilder::default()
        .default_models(false)
        .plugin(plugin)
        .plugin(AgentLoopPlugin::new(Invoke {
            name,
            args,
            output: output.clone(),
            cancel,
        }))
        .build()
        .await
        .unwrap();
    let run = agent.run(RunRequest::new("tools", "invoke")).await;
    assert!(agent.close().await.is_success());
    let value = output.lock().unwrap().take();
    value.unwrap_or_else(|| Err(run.expect_err("missing tool must fail").to_string()))
}
