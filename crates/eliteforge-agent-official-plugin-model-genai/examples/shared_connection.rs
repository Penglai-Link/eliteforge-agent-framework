//! A host configures credentials once, then selects models by phase.
//! This example performs real API calls only when explicitly run.
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_model_genai::*;
use eliteforge_agent_state_sqlite::{SqliteOptions, SqliteStateStore};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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
    // Alternatively call provider.discover().await, then apply host authorization.
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
    let state = Arc::new(
        SqliteStateStore::open(
            std::env::var("STATE_DB").unwrap_or_else(|_| ":memory:".into()),
            SqliteOptions::default(),
        )
        .await?,
    );
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
            eliteforge_agent_official_plugin_storage_sqlite::SqliteStoragePlugin::new(
                "agent.sqlite",
            ),
        )
        .plugin(state.clone().plugin())
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
    state.close().await?;
    println!("{}", outcome?.output);
    if !report.is_success() {
        return Err(format!("shutdown failed: {report:?}").into());
    }
    Ok(())
}
