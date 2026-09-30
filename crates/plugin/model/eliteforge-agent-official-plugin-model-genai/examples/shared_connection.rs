//! A host configures credentials once, then selects models by phase.
//! This example performs real API calls only when explicitly run.
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_model_genai::*;
use std::sync::Arc;
#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let artifact_root = std::env::var("ARTIFACT_ROOT").unwrap_or_else(|_| "agent-artifacts".into());
    let mut artifacts = std::fs::DirBuilder::new();
    artifacts.recursive(true);
    #[cfg(unix)]
    artifacts.mode(0o700);
    artifacts.create(&artifact_root)?;
    let endpoint = std::env::var("MODEL_ENDPOINT")?;
    let planner = std::env::var("PLANNER_MODEL")?;
    let executor = std::env::var("EXECUTOR_MODEL")?;
    let provider = Arc::new(GenaiChatAdapter::new(
        "subscription",
        AdapterKind::OpenAI,
        endpoint,
        AuthData::from_env("MODEL_API_KEY"),
    )?);
    let mut ids = vec![planner.clone(), executor.clone()];
    ids.sort();
    ids.dedup();
    let strategy = StageModelStrategy {
        stages: [
            (
                "plan".into(),
                Arc::new(FixedModelStrategy(ModelRef::new("subscription", planner)))
                    as Arc<dyn ModelSelectionStrategy>,
            ),
            (
                "execute".into(),
                Arc::new(RoutingStrategy {
                    candidates: vec![ModelRef::new("subscription", executor)],
                }),
            ),
        ]
        .into(),
        fallback: None,
    };
    let mut plan = WorkflowStage::new("plan");
    plan.instructions.push(Message::system(
        "Create a concise implementation plan. The next stage will implement it.",
    ));
    let mut execute = WorkflowStage::new("execute");
    execute.instructions.push(Message::system(
        "Implement the previous stage's plan and return the Rust source.",
    ));
    let agent = AgentBuilder::new()
        .plugin(
            eliteforge_agent_official_plugin_storage_local::LocalStoragePlugin::new(
                std::env::var("STATE_DB").unwrap_or_else(|_| "agent.sqlite".into()),
                artifact_root,
            ),
        )
        .plugin(AgentLoopPlugin::new(WorkflowLoop::new(
            WorkflowDefinition {
                id: "shared-model-phases".into(),
                version: 1,
                stages: vec![plan, execute],
            },
        )?))
        .plugin(
            eliteforge_agent_official_plugin_model_genai::GenaiPlugin::from_adapter(
                provider,
                ids.into_iter().map(ModelDescriptor::new).collect(),
            ),
        )
        .plugin(ModelStrategyPlugin(Arc::new(strategy)))
        .build()
        .await?;
    let outcome = agent
        .run(RunRequest::new(
            "demo",
            "Create a small Rust CLI that adds two integers.",
        ))
        .await;
    let report = agent.close().await;
    println!("{}", outcome?.output);
    if !report.is_success() {
        return Err(format!("shutdown failed: {report:?}").into());
    }
    Ok(())
}
