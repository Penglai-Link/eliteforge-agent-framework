//! Offline AG-UI SSE example. Redirect stdout to inspect/replay the wire frames.
use agent_agui::AgUiPlugin;
use agent_framework::*;
use async_trait::async_trait;

struct DemoModel;
#[async_trait]
impl Model for DemoModel {
    async fn generate(
        &self,
        _: ModelRequest,
        stream: &dyn ModelStream,
    ) -> Result<ModelResponse, ModelError> {
        stream.reasoning_delta("生成一个简短回复。").await;
        stream.text_delta("你好，").await;
        stream.text_delta("这是 AG-UI 事件流。").await;
        Ok(ModelResponse::text("你好，这是 AG-UI 事件流。"))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let agui = AgUiPlugin::default();
    let agent = AgentBuilder::new()
        .plugin(agent_storage_sqlite::SqliteStoragePlugin::new(
            "agent.sqlite",
        ))
        .plugin(ModelPlugin::new(DemoModel))
        .plugin(agui.clone())
        .build()
        .await?;
    let (handle, mut events) = agui.start(&agent, RunRequest::new("chat-1", "你好"))?;
    let control = handle.control();
    let consume = async {
        while let Some(frame) = events.recv_sse().await? {
            // An HTTP handler writes each frame to its text/event-stream body.
            print!("{frame}");
        }
        Ok::<(), agent_agui::ag_ui::Error>(())
    };
    let (result, delivered) = tokio::join!(handle.wait(), consume);
    let shutdown = agent.close().await;
    delivered?;
    result?;
    eprintln!("run {} finished", control.run_id());
    if !shutdown.is_success() {
        return Err(format!("shutdown failed: {shutdown:?}").into());
    }
    Ok(())
}
