//! Offline official-pack example with local storage and AG-UI output.
use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_capability_pack::{
    EliteForgeOfficialCapabilityPack, agui::AgUiStream,
};

struct DemoModel;
#[async_trait]
impl ChatModel for DemoModel {
    async fn generate(
        &self,
        _: &str,
        _: ChatRequest,
        stream: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        stream
            .text_delta("插件成组配置，操作统一通过 Agent。")
            .await;
        Ok(ChatResponse::text("插件成组配置，操作统一通过 Agent。"))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let database = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "agent.sqlite".into());
    let artifact_root = std::path::Path::new(&database).with_extension("artifacts");
    std::fs::create_dir_all(&artifact_root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&artifact_root, std::fs::Permissions::from_mode(0o700))?;
    }
    let framework = FrameworkBuilder::new()
        .capability_pack(
            EliteForgeOfficialCapabilityPack::new(database, artifact_root)
                .model(ChatModelPlugin::new(DemoModel)),
        )
        .build()
        .await?;
    let agent = framework.agent(AgentSelection::default()).build().await?;
    let (handle, mut events) =
        agent.start_with_output::<AgUiStream>(RunRequest::new("chat-1", "介绍插件包"))?;
    let consume = async {
        while let Some(frame) = events.recv_sse().await? {
            print!("{frame}");
        }
        Ok::<(), eliteforge_agent_official_capability_pack::agui::ag_ui::Error>(())
    };
    let (result, delivered) = tokio::join!(agent.wait(handle), consume);
    let shutdown = framework.close().await;
    result?;
    delivered?;
    if !shutdown.is_success() {
        return Err(format!("shutdown failed: {shutdown:?}").into());
    }
    Ok(())
}
