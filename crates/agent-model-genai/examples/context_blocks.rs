//! Explicit block quotas and prompt order. Running this example calls the configured API.
use agent_framework::*;
use agent_model_genai::{AdapterKind, AuthData, GenaiProvider};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let provider = Arc::new(GenaiProvider::new(
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
        .plugin(agent_storage_sqlite::SqliteStoragePlugin::new(
            "agent.sqlite",
        ))
        .plugin(provider.plugin(vec![ModelDescriptor::new(std::env::var("MODEL")?)]))
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
