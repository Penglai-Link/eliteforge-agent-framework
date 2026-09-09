//! MODEL selects the default chat model; credentials use the provider's environment variable.
use eliteforge_agent_framework::{AgentBuilder, RunRequest};
use eliteforge_agent_official_plugin_storage_sqlite::SqliteStoragePlugin;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let agent = AgentBuilder::new()
        .plugin(SqliteStoragePlugin::new("agent.sqlite"))
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
