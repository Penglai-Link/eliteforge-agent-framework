mod orders;

use eliteforge_agent_framework::{AgentSelection, FrameworkBuilder, RunRequest};
use eliteforge_agent_official_capability_pack::{
    EliteForgeOfficialCapabilityPack,
    model::{AdapterKind, AuthData, GenaiPlugin},
};
use orders::{ShopConfig, shop_pack};
use std::{collections::BTreeMap, sync::Arc};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let model = std::env::var("ELITEFORGE_SKILL_AGENT_FRAMEWORK_MODEL")?;
    let endpoint = std::env::var("ELITEFORGE_SKILL_AGENT_FRAMEWORK_ENDPOINT")?;
    let question = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "查询订单 A100 的状态".into());
    std::fs::create_dir_all("agent-data/artifacts")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            "agent-data/artifacts",
            std::fs::Permissions::from_mode(0o700),
        )?;
    }
    let framework = FrameworkBuilder::new()
        .default_models(false)
        .capability_pack(
            EliteForgeOfficialCapabilityPack::new(
                "agent-data/state.sqlite",
                "agent-data/artifacts",
            )
            .model(
                GenaiPlugin::new(model)
                    .adapter(AdapterKind::OpenAI)
                    .endpoint(endpoint)
                    .auth(AuthData::from_env(
                        "ELITEFORGE_SKILL_AGENT_FRAMEWORK_API_KEY",
                    )),
            ),
        )
        .capability_pack(shop_pack(ShopConfig {
            name: "星河商店".into(),
            orders: Arc::new(BTreeMap::from([("A100".into(), "已发货".into())])),
        }))
        .build()
        .await?;
    let outcome = async {
        let agent = framework
            .agent(AgentSelection {
                tool_providers: Some(vec!["shop.orders".into()]),
                tools: Some(vec!["shop.orders.lookup".into()]),
                context_sources: Some(vec!["shop.instructions".into()]),
                ..Default::default()
            })
            .build()
            .await?;
        let mut request = RunRequest::new("shop-demo", question);
        request.limits.max_steps = 8;
        agent.run(request).await
    }
    .await;
    let shutdown = framework.close().await;
    let result = outcome?;
    if !shutdown.is_success() {
        return Err(format!("关闭失败：{shutdown:?}").into());
    }
    println!("状态：{:?}\n{}", result.status, result.output);
    Ok(())
}
