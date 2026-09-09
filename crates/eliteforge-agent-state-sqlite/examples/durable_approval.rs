//! Standalone durable approval with a deterministic protocol fixture, not an LLM.
//! Each command is a separate process. Replace Arithmetic with a ChatModel
//! plugin (e.g. genai) to drive the same workflow with an actual model.
use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_storage_sqlite::SqliteStoragePlugin;
use serde_json::{Value, json};

struct Arithmetic;
#[async_trait]
impl ChatModel for Arithmetic {
    async fn generate(
        &self,
        _model_id: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        if let Some(result) = request
            .messages
            .iter()
            .rev()
            .take_while(|m| m.role != Role::User)
            .find(|m| m.role == Role::Tool)
        {
            Ok(ChatResponse::text(result.content.protocol_text()))
        } else {
            let amount: i64 = request
                .messages
                .iter()
                .rev()
                .find(|m| m.role == Role::User)
                .ok_or_else(|| ModelError::Fatal("missing amount".into()))?
                .content
                .text()
                .parse()
                .map_err(|_| ModelError::Fatal("amount must be an integer".into()))?;
            Ok(ChatResponse::tool_calls(vec![ToolCall::new(
                "double",
                "double",
                json!({"amount":amount}),
            )]))
        }
    }
}
struct Double;
#[async_trait]
impl Tool for Double {
    async fn call(&self, args: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        let amount = args["amount"]
            .as_i64()
            .ok_or_else(|| ToolError::Failed("integer required".into()))?;
        let doubled = amount
            .checked_mul(2)
            .ok_or_else(|| ToolError::Failed("integer overflow".into()))?;
        Ok(ToolOutput::text(doubled.to_string()))
    }
}
struct Approval;
#[async_trait]
impl ToolPolicy for Approval {
    async fn evaluate(
        &self,
        invocation: &ToolInvocationContext<'_>,
    ) -> Result<ToolPolicyDecision, AgentError> {
        Ok(ToolPolicyDecision::RequireApproval(
            format!("Approve doubling {}?", invocation.call.arguments["amount"]).into(),
        ))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let mut request = match args.get(1).map(String::as_str) {
        Some("start") if args.len() == 4 => RunRequest::new("demo", args[3].clone()),
        Some("answer") if args.len() == 6 => {
            let mut request = RunRequest::new("demo", "");
            request.resume = Some(ResumeRequest { run_id:args[3].parse()?, response:Some(InteractionResponse { interaction_id:args[4].parse()?, value:json!(args[5].parse::<bool>()?) }), resolutions:vec![] });
            request
        },
        _ => return Err("usage: durable_approval start <database> <amount> | answer <database> <run UUID> <interaction UUID> <true|false>".into()),
    };
    request.limits.max_steps = 4;
    let agent = AgentBuilder::new().plugin(SqliteStoragePlugin::new(&args[2])).plugin(ChatModelPlugin::new(Arithmetic))
        .plugin(AgentLoopPlugin::new(WorkflowLoop::new(WorkflowDefinition { id:"approval-example".into(), version:1, stages:vec![WorkflowStage::new("calculate")] })?))
        .plugin(ToolPolicyPlugin::new("approval", Approval))
        .plugin(ToolProviderPlugin::new("tools", StaticToolProvider::new(vec![ToolBinding::new(ToolDescriptor::new("double", "double", "Double an integer", json!({"type":"object","properties":{"amount":{"type":"integer"}},"required":["amount"],"additionalProperties":false})), Double)])))
        .build().await?;
    let outcome = async {
        let handle = agent.start(request)?;
        let run_id = handle.run_id;
        let result = agent.wait(handle).await?;
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"run_id":run_id,"result":result}))?
        );
        Ok::<_, Box<dyn std::error::Error>>(())
    }
    .await;
    let report = agent.close().await;
    if !report.is_success() {
        return Err(format!("shutdown failed: {report:?}").into());
    }
    outcome
}
