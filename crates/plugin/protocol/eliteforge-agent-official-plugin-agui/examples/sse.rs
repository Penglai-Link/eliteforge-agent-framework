//! Offline AG-UI SSE example. Redirect stdout to inspect/replay the wire frames.
use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_agui::{AgUiPlugin, AgUiStream};
#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;

struct DemoModel;
#[async_trait]
impl ChatModel for DemoModel {
    async fn generate(
        &self,
        _model_id: &str,
        _: ChatRequest,
        stream: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        stream.reasoning_delta("生成一个简短回复。").await;
        stream.text_delta("你好，").await;
        stream.text_delta("这是 AG-UI 事件流。").await;
        Ok(ChatResponse::text("你好，这是 AG-UI 事件流。"))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut artifacts = std::fs::DirBuilder::new();
    artifacts.recursive(true);
    #[cfg(unix)]
    artifacts.mode(0o700);
    artifacts.create("agent-artifacts")?;
    let agent = AgentBuilder::new()
        .plugin(
            eliteforge_agent_official_plugin_storage_local::LocalStoragePlugin::new(
                "agent.sqlite",
                "agent-artifacts",
            ),
        )
        .plugin(ChatModelPlugin::new(DemoModel))
        .plugin(AgUiPlugin::default())
        .build()
        .await?;
    let (handle, mut events) =
        agent.start_with_output::<AgUiStream>(RunRequest::new("chat-1", "你好"))?;
    let control = agent.control(&handle)?;
    let consume = async {
        while let Some(frame) = events.recv_sse().await? {
            // An HTTP handler writes each frame to its text/event-stream body.
            print!("{frame}");
        }
        Ok::<(), eliteforge_agent_official_plugin_agui::ag_ui::Error>(())
    };
    let (result, delivered) = tokio::join!(agent.wait(handle), consume);
    let shutdown = agent.close().await;
    delivered?;
    result?;
    eprintln!("run {} finished", control.run_id());
    if !shutdown.is_success() {
        return Err(format!("shutdown failed: {shutdown:?}").into());
    }
    Ok(())
}
