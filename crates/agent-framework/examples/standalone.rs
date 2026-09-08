//! A complete offline host: model -> tool -> observation -> final answer.
//! Replace DemoModel with your own HTTP/SDK adapter to use a real model.

use agent_framework::{
    AgentBuilder, Model, ModelError, ModelPlugin, ModelRequest, ModelResponse, ModelStream, Role,
    RunContext, RunRequest, StaticToolProvider, Tool, ToolBinding, ToolCall, ToolDescriptor,
    ToolError, ToolOutput, ToolProviderPlugin,
};
use async_trait::async_trait;
use serde_json::{Value, json};

struct DemoModel;

#[async_trait]
impl Model for DemoModel {
    async fn generate(
        &self,
        request: ModelRequest,
        stream: &dyn ModelStream,
    ) -> Result<ModelResponse, ModelError> {
        if !request
            .messages
            .iter()
            .rev()
            .take_while(|message| message.role != Role::User)
            .any(|message| message.role == Role::Tool)
        {
            let input = request
                .messages
                .iter()
                .rev()
                .find(|message| message.role == Role::User)
                .ok_or_else(|| ModelError::Fatal("missing input".into()))?;
            let arguments = serde_json::from_str(&input.content.text())
                .map_err(|_| ModelError::Fatal("demo expects JSON operands".into()))?;
            Ok(ModelResponse::tool_calls(vec![ToolCall::new(
                "call-1",
                "calculator_add",
                arguments,
            )]))
        } else {
            let observation = request
                .messages
                .iter()
                .rev()
                .find(|message| message.role == Role::Tool)
                .ok_or_else(|| ModelError::Fatal("expected a tool observation".into()))?;
            let answer = format!("sum = {}", observation.content);
            stream.text_delta(&answer).await;
            Ok(ModelResponse::text(answer))
        }
    }
}

struct Add;

#[async_trait]
impl Tool for Add {
    async fn call(&self, arguments: Value, _context: &RunContext) -> Result<ToolOutput, ToolError> {
        // Framework validates the JSON Schema before dispatch; domain overflow
        // checks remain the tool implementation's responsibility.
        let left = arguments["left"]
            .as_i64()
            .ok_or_else(|| ToolError::Failed("left must be an integer".into()))?;
        let right = arguments["right"]
            .as_i64()
            .ok_or_else(|| ToolError::Failed("right must be an integer".into()))?;
        let sum = left
            .checked_add(right)
            .ok_or_else(|| ToolError::Failed("integer overflow".into()))?;
        Ok(ToolOutput::text(sum.to_string()))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 {
        return Err("usage: standalone <left integer> <right integer>".into());
    }
    let left: i64 = args[1].parse()?;
    let right: i64 = args[2].parse()?;
    let tools = StaticToolProvider::new(vec![ToolBinding::new(
        ToolDescriptor::new(
            "calculator.add",
            "calculator_add",
            "Add two integers",
            json!({
                "type": "object",
                "properties": {
                    "left": { "type": "integer" },
                    "right": { "type": "integer" }
                },
                "required": ["left", "right"],
                "additionalProperties": false
            }),
        ),
        Add,
    )]);

    let agent = AgentBuilder::new()
        .plugin(agent_storage_sqlite::SqliteStoragePlugin::new(
            "agent.sqlite",
        ))
        .plugin(ModelPlugin::new(DemoModel))
        .plugin(ToolProviderPlugin::new("demo.tools", tools))
        .build()
        .await?;

    let result = agent
        .run(RunRequest::new(
            "demo-session",
            json!({"left":left,"right":right}).to_string(),
        ))
        .await?;
    println!("{}", result.output);
    println!(
        "Model steps: {}, tool calls: {}",
        result.steps, result.tool_calls
    );
    agent.close().await;
    Ok(())
}
