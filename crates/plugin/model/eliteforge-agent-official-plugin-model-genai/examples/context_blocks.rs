//! Explicit block quotas and prompt order. Running this example calls the configured API.
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_model_genai::{AdapterKind, AuthData, GenaiChatAdapter};
#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut artifacts = std::fs::DirBuilder::new();
    artifacts.recursive(true);
    #[cfg(unix)]
    artifacts.mode(0o700);
    artifacts.create("agent-artifacts")?;
    let provider = Arc::new(GenaiChatAdapter::new(
        "default",
        AdapterKind::OpenAI,
        std::env::var("MODEL_ENDPOINT")?,
        AuthData::from_env("MODEL_API_KEY"),
    )?);
    let layout = ContextLayout(vec![
        ContextBlockConfig::new(INSTRUCTIONS_BLOCK, BlockBudget::tokens(1000)),
        ContextBlockConfig::new("task", BlockBudget::tokens(2000)),
        ContextBlockConfig::new(HISTORY_BLOCK, BlockBudget::tokens(9000))
            .reduce_with(RecentConversation),
    ]);
    let agent = AgentBuilder::new()
        .plugin(
            eliteforge_agent_official_plugin_storage_local::LocalStoragePlugin::new(
                "agent.sqlite",
                "agent-artifacts",
            ),
        )
        .plugin(
            eliteforge_agent_official_plugin_model_genai::GenaiPlugin::from_adapter(
                provider,
                vec![ModelDescriptor::new(std::env::var("MODEL")?)],
            ),
        )
        .plugin(ContextPlugin::new(
            "prompt",
            FixedContext::new("用简短的中文回答。"),
        ))
        .plugin(ContextContributorPlugin::new(
            "task-data",
            FixedContextContributor(ContextBlock::data(
                "task",
                "当前任务：设计一个 Rust 视频剪辑 Agent。",
            )),
        ))
        .plugin(ContextLayoutPlugin(layout))
        .build()
        .await?;
    let mut request = RunRequest::new("blocks-1", "先列出三个实现步骤");
    request.limits.max_context_tokens = 12_000;
    let result = agent.run(request).await;
    let shutdown = agent.close().await;
    println!("{}", result?.output);
    if !shutdown.is_success() {
        return Err(format!("shutdown failed: {shutdown:?}").into());
    }
    Ok(())
}
