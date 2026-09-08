//! Default ReAct Agent using one genai model connection.
//! Running this example calls MODEL_ENDPOINT with MODEL_API_KEY.
use agent_framework::{AgentBuilder, ModelDescriptor, RunRequest};
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

    let agent = AgentBuilder::new()
        .plugin(agent_storage_sqlite::SqliteStoragePlugin::new(
            "agent.sqlite",
        ))
        .plugin(provider.plugin(vec![ModelDescriptor::new(std::env::var("MODEL")?)]))
        .build()
        .await?;

    let result = agent
        .run(RunRequest::new("chat-1", "你好，请介绍一下自己"))
        .await;
    let shutdown = agent.close().await;
    println!("{}", result?.output);
    if !shutdown.is_success() {
        return Err(format!("shutdown failed: {shutdown:?}").into());
    }
    Ok(())
}
