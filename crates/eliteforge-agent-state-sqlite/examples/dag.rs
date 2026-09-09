//! Offline DAG example: route by sign, transform the selected branch, then join.
//! cargo run -p eliteforge-agent-state-sqlite --example dag -- /tmp/dag-demo.db -7
use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_storage_sqlite::SqliteStoragePlugin;
use serde_json::json;

struct ArithmeticNodes;
#[async_trait]
impl WorkflowNode for ArithmeticNodes {
    fn kind(&self) -> &str {
        "example.arithmetic"
    }
    fn supports_tool_policy(&self) -> bool {
        true
    }
    async fn execute(
        &self,
        input: WorkflowNodeInput,
        _: &WorkflowNodeContext,
        _: &AgentServices,
    ) -> Result<WorkflowNodeResult, AgentError> {
        if input.node_id == "route" {
            let number: i64 = input
                .request
                .input
                .text()
                .parse()
                .map_err(|_| AgentError::Run("input must be an integer".into()))?;
            let mut result = WorkflowNodeResult::completed(json!(number));
            result.routes =
                Some([if number < 0 { "negative" } else { "positive" }.to_owned()].into());
            return Ok(result);
        }
        let number = input
            .predecessors
            .values()
            .next()
            .and_then(|v| v.as_i64())
            .ok_or_else(|| AgentError::Run("missing predecessor number".into()))?;
        Ok(WorkflowNodeResult::completed(json!(
            match input.node_id.as_str() {
                "positive" => number.checked_mul(2),
                "negative" => number.checked_neg(),
                _ => Some(number),
            }
            .ok_or_else(|| AgentError::Run("integer overflow".into()))?
        )))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let path = args.get(1).ok_or("usage: dag DATABASE INTEGER")?;
    let input = args.get(2).ok_or("usage: dag DATABASE INTEGER")?;
    let definition = DagWorkflowDefinition {
        id: "arithmetic-dag".into(),
        version: 1,
        max_parallel_nodes: 2,
        nodes: ["route", "positive", "negative", "join"]
            .into_iter()
            .map(|id| DagNode::new(id, "example.arithmetic", json!({})))
            .collect(),
        edges: vec![
            DagEdge::new("route", "positive"),
            DagEdge::new("route", "negative"),
            DagEdge::new("positive", "join"),
            DagEdge::new("negative", "join"),
        ],
    };
    let agent = AgentBuilder::new()
        .plugin(SqliteStoragePlugin::new(path))
        .plugin(WorkflowNodePlugin::new("example.nodes", ArithmeticNodes))
        .plugin(AgentLoopPlugin::new(DagWorkflowLoop::new(definition)?))
        .build()
        .await?;
    let handle = agent.start(RunRequest::new("arithmetic", input))?;
    let run_id = handle.run_id;
    let result = agent.wait(handle).await?;
    println!(
        "run={run_id}, result={}",
        result.structured.unwrap_or_default()
    );
    let state: DagWorkflowState = serde_json::from_value(agent.checkpoint(run_id).await?.state)?;
    for (id, node) in state.nodes {
        println!("{id}: {:?}", node.status);
    }
    agent.close().await;
    Ok(())
}
