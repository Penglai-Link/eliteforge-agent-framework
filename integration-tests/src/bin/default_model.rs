//! Live-model smoke program; run explicitly with the live-model feature.
//! MODEL selects the default chat model; credentials use the provider's environment variable.
use eliteforge_agent_framework::{AgentBuilder, RunRequest};
use eliteforge_agent_official_plugin_storage_local::LocalStoragePlugin;
#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut artifacts = std::fs::DirBuilder::new();
    artifacts.recursive(true);
    #[cfg(unix)]
    artifacts.mode(0o700);
    artifacts.create("agent-artifacts")?;
    let agent = AgentBuilder::new()
        .plugin(LocalStoragePlugin::new("agent.sqlite", "agent-artifacts"))
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
