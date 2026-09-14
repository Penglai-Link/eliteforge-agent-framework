//! Official model-callable tools for one-shot delegation and independent threads.
//!
//! The plugin is deliberately a thin protocol adapter. The runtime bridge is
//! resolved from the current [`RunContext`], so applications can replace the
//! default in-process implementation with an authenticated remote coordinator.
use async_trait::async_trait;
use eliteforge_agent_core::{
    AgentCollaborationRuntime, AgentCollaborationService, AgentError, ChildRunResult, Content,
    Plugin, PluginContext, PluginError, RunContext, RunId, RunLimits, RunRequest,
    StaticToolProvider, ThreadSummary, ThreadView, Tool, ToolBinding, ToolDescriptor, ToolError,
    ToolOutput, ToolProvider, ToolProviderService,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

const MAX_TASK_BYTES: usize = 16 * 1024;
const MAX_THREAD_ID_BYTES: usize = 256;
const MAX_READ_MESSAGES: usize = 100;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnInput {
    task: String,
    #[serde(default)]
    max_steps: Option<usize>,
    #[serde(default)]
    max_total_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateThreadInput {
    task: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    max_steps: Option<usize>,
    #[serde(default)]
    max_total_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SendThreadInput {
    thread_id: String,
    message: String,
    #[serde(default)]
    max_steps: Option<usize>,
    #[serde(default)]
    max_total_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerThreadInput {
    thread_id: String,
    interaction_id: RunId,
    answer: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitThreadInput {
    thread_id: String,
    #[serde(default)]
    run_id: Option<RunId>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadThreadInput {
    thread_id: String,
    #[serde(default = "default_read_limit")]
    limit: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListThreadsInput {
    #[serde(default = "default_list_limit")]
    limit: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CloseThreadInput {
    thread_id: String,
}

fn default_read_limit() -> usize {
    20
}

fn default_list_limit() -> usize {
    50
}

fn task(value: String) -> Result<String, ToolError> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_TASK_BYTES {
        return Err(ToolError::Failed(
            "task/message must be nonempty and at most 16384 bytes".into(),
        ));
    }
    Ok(value)
}

fn thread_id(value: String) -> Result<String, ToolError> {
    if value.trim().is_empty() || value.len() > MAX_THREAD_ID_BYTES {
        return Err(ToolError::Failed(
            "thread_id must be nonempty and at most 256 bytes".into(),
        ));
    }
    Ok(value)
}

fn limits(
    max_steps: Option<usize>,
    max_total_tokens: Option<u64>,
    context: &RunContext,
) -> Result<RunLimits, ToolError> {
    let mut limits = context.limits().clone();
    if let Some(value) = max_steps {
        if value == 0 || value > limits.max_steps {
            return Err(ToolError::Failed(format!(
                "max_steps must be between 1 and {}",
                limits.max_steps
            )));
        }
        limits.max_steps = value;
    }
    if let Some(value) = max_total_tokens {
        if value == 0 || value > limits.max_total_tokens {
            return Err(ToolError::Failed(format!(
                "max_total_tokens must be between 1 and {}",
                limits.max_total_tokens
            )));
        }
        limits.max_total_tokens = value;
    }
    Ok(limits)
}

fn request(input: impl Into<Content>, limits: RunLimits) -> RunRequest {
    let mut request = RunRequest::new(format!("tool-run-{}", RunId::new_v4()), input);
    request.limits = limits;
    request
}

fn runtime(context: &RunContext) -> Result<Arc<dyn AgentCollaborationRuntime>, ToolError> {
    context
        .extensions
        .get::<AgentCollaborationService>()
        .map(|service| service.0.clone())
        .ok_or_else(|| ToolError::Failed("agent collaboration runtime is unavailable".into()))
}

fn failed(error: AgentError) -> ToolError {
    ToolError::Failed(error.to_string())
}

fn structured(value: Value) -> ToolOutput {
    let text = serde_json::to_string(&value).expect("JSON value is serializable");
    let mut output = ToolOutput::text(text);
    output.structured = Some(value);
    output
}

fn spawn_result(child: ChildRunResult) -> ToolOutput {
    let run_id = child.run_id;
    let result = child.result;
    structured(json!({
        "run_id": run_id,
        "status": result.status,
        "output": result.output,
        "content": result.content,
        "structured": result.structured,
        "interactions": result.interactions,
        "usage": result.usage,
        "steps": result.steps,
        "tool_calls": result.tool_calls,
    }))
}

fn thread_summary(summary: ThreadSummary) -> ToolOutput {
    structured(serde_json::to_value(summary).expect("thread summary is serializable"))
}

pub struct AgentSpawnTool;

impl AgentSpawnTool {
    pub fn descriptor() -> ToolDescriptor {
        let ceilings = RunLimits::default();
        ToolDescriptor::new(
            "agent.spawn",
            "agent_spawn",
            "Delegate one focused task to a child agent and wait for its result.",
            json!({
                "type":"object","additionalProperties":false,"required":["task"],"properties":{
                    "task":{"type":"string","minLength":1,"maxLength":16384},
                    "max_steps":{"type":"integer","minimum":1,"maximum":ceilings.max_steps},
                    "max_total_tokens":{"type":"integer","minimum":1,"maximum":ceilings.max_total_tokens}
                }
            }),
        )
    }
}

#[async_trait]
impl Tool for AgentSpawnTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let input: SpawnInput = serde_json::from_value(arguments)
            .map_err(|_| ToolError::Failed("invalid agent_spawn arguments".into()))?;
        let task = task(input.task)?;
        let runtime = runtime(context)?;
        let result = runtime
            .spawn_child(
                request(
                    task,
                    limits(input.max_steps, input.max_total_tokens, context)?,
                ),
                context,
            )
            .await
            .map_err(failed)?;
        Ok(spawn_result(result))
    }
}

pub struct ThreadCreateTool;

impl ThreadCreateTool {
    pub fn descriptor() -> ToolDescriptor {
        let ceilings = RunLimits::default();
        ToolDescriptor::new(
            "thread.create",
            "thread_create",
            "Start an independent, addressable agent thread. Returns immediately while it runs.",
            json!({
                "type":"object","additionalProperties":false,"required":["task"],"properties":{
                    "task":{"type":"string","minLength":1,"maxLength":16384},
                    "title":{"type":"string","minLength":1,"maxLength":200},
                    "max_steps":{"type":"integer","minimum":1,"maximum":ceilings.max_steps},
                    "max_total_tokens":{"type":"integer","minimum":1,"maximum":ceilings.max_total_tokens}
                }
            }),
        )
    }
}

#[async_trait]
impl Tool for ThreadCreateTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let input: CreateThreadInput = serde_json::from_value(arguments)
            .map_err(|_| ToolError::Failed("invalid thread_create arguments".into()))?;
        let task = task(input.task)?;
        if input
            .title
            .as_ref()
            .is_some_and(|title| title.trim().is_empty())
        {
            return Err(ToolError::Failed("title must not be blank".into()));
        }
        let runtime = runtime(context)?;
        let run = request(
            task,
            limits(input.max_steps, input.max_total_tokens, context)?,
        );
        Ok(thread_summary(
            runtime
                .create_thread(run, input.title, context)
                .await
                .map_err(failed)?,
        ))
    }
}

pub struct ThreadSendTool;

impl ThreadSendTool {
    pub fn descriptor() -> ToolDescriptor {
        let ceilings = RunLimits::default();
        ToolDescriptor::new(
            "thread.send",
            "thread_send",
            "Send a new message to an existing independent thread and start its next run.",
            json!({
                "type":"object","additionalProperties":false,"required":["thread_id","message"],"properties":{
                    "thread_id":{"type":"string","minLength":1,"maxLength":256},
                    "message":{"type":"string","minLength":1,"maxLength":16384},
                    "max_steps":{"type":"integer","minimum":1,"maximum":ceilings.max_steps},
                    "max_total_tokens":{"type":"integer","minimum":1,"maximum":ceilings.max_total_tokens}
                }
            }),
        )
    }
}

#[async_trait]
impl Tool for ThreadSendTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let input: SendThreadInput = serde_json::from_value(arguments)
            .map_err(|_| ToolError::Failed("invalid thread_send arguments".into()))?;
        let runtime = runtime(context)?;
        let summary = runtime
            .send_thread(
                &thread_id(input.thread_id)?,
                request(
                    task(input.message)?,
                    limits(input.max_steps, input.max_total_tokens, context)?,
                ),
                context,
            )
            .await
            .map_err(failed)?;
        Ok(thread_summary(summary))
    }
}

pub struct ThreadAnswerTool;

impl ThreadAnswerTool {
    pub fn descriptor() -> ToolDescriptor {
        ToolDescriptor::new(
            "thread.answer",
            "thread_answer",
            "Answer a pending interaction and resume the independent thread.",
            json!({
                "type":"object","additionalProperties":false,
                "required":["thread_id","interaction_id","answer"],"properties":{
                    "thread_id":{"type":"string","minLength":1,"maxLength":256},
                    "interaction_id":{"type":"string","format":"uuid"},
                    "answer":{}
                }
            }),
        )
    }
}

#[async_trait]
impl Tool for ThreadAnswerTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let input: AnswerThreadInput = serde_json::from_value(arguments)
            .map_err(|_| ToolError::Failed("invalid thread_answer arguments".into()))?;
        let summary = runtime(context)?
            .answer_thread(
                &thread_id(input.thread_id)?,
                input.interaction_id,
                input.answer,
                context,
            )
            .await
            .map_err(failed)?;
        Ok(thread_summary(summary))
    }
}

pub struct ThreadWaitTool;

impl ThreadWaitTool {
    pub fn descriptor() -> ToolDescriptor {
        ToolDescriptor::new(
            "thread.wait",
            "thread_wait",
            "Wait for an independent thread run and return its result.",
            json!({
                "type":"object","additionalProperties":false,"required":["thread_id"],"properties":{
                    "thread_id":{"type":"string","minLength":1,"maxLength":256},
                    "run_id":{"type":"string","format":"uuid"}
                }
            }),
        )
    }
}

#[async_trait]
impl Tool for ThreadWaitTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let input: WaitThreadInput = serde_json::from_value(arguments)
            .map_err(|_| ToolError::Failed("invalid thread_wait arguments".into()))?;
        let result = runtime(context)?
            .wait_thread(&thread_id(input.thread_id)?, input.run_id, context)
            .await
            .map_err(failed)?;
        Ok(structured(
            serde_json::to_value(result).expect("thread result is serializable"),
        ))
    }
}

pub struct ThreadReadTool;

impl ThreadReadTool {
    pub fn descriptor() -> ToolDescriptor {
        ToolDescriptor::new(
            "thread.read",
            "thread_read",
            "Read recent messages and status from an independent thread.",
            json!({
                "type":"object","additionalProperties":false,"required":["thread_id"],"properties":{
                    "thread_id":{"type":"string","minLength":1,"maxLength":256},
                    "limit":{"type":"integer","minimum":1,"maximum":100}
                }
            }),
        )
    }
}

#[async_trait]
impl Tool for ThreadReadTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let input: ReadThreadInput = serde_json::from_value(arguments)
            .map_err(|_| ToolError::Failed("invalid thread_read arguments".into()))?;
        if input.limit == 0 || input.limit > MAX_READ_MESSAGES {
            return Err(ToolError::Failed("limit must be between 1 and 100".into()));
        }
        let value: ThreadView = runtime(context)?
            .read_thread(&thread_id(input.thread_id)?, input.limit)
            .await
            .map_err(failed)?;
        Ok(structured(
            serde_json::to_value(value).expect("thread view is serializable"),
        ))
    }
}

pub struct ThreadListTool;

impl ThreadListTool {
    pub fn descriptor() -> ToolDescriptor {
        ToolDescriptor::new(
            "thread.list",
            "thread_list",
            "List independent threads available to the current agent identity.",
            json!({
                "type":"object","additionalProperties":false,"properties":{
                    "limit":{"type":"integer","minimum":1,"maximum":100}
                }
            }),
        )
    }
}

#[async_trait]
impl Tool for ThreadListTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let input: ListThreadsInput = serde_json::from_value(arguments)
            .map_err(|_| ToolError::Failed("invalid thread_list arguments".into()))?;
        if input.limit == 0 || input.limit > MAX_READ_MESSAGES {
            return Err(ToolError::Failed("limit must be between 1 and 100".into()));
        }
        let mut threads = runtime(context)?
            .list_threads(context)
            .await
            .map_err(failed)?;
        threads.truncate(input.limit);
        Ok(structured(json!({"threads": threads})))
    }
}

pub struct ThreadCloseTool;

impl ThreadCloseTool {
    pub fn descriptor() -> ToolDescriptor {
        ToolDescriptor::new(
            "thread.close",
            "thread_close",
            "Close a finished or paused independent thread and release its in-process registry capacity.",
            json!({
                "type":"object","additionalProperties":false,"required":["thread_id"],"properties":{
                    "thread_id":{"type":"string","minLength":1,"maxLength":256}
                }
            }),
        )
    }
}

#[async_trait]
impl Tool for ThreadCloseTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let input: CloseThreadInput = serde_json::from_value(arguments)
            .map_err(|_| ToolError::Failed("invalid thread_close arguments".into()))?;
        let thread_id = thread_id(input.thread_id)?;
        runtime(context)?
            .close_thread(&thread_id, context)
            .await
            .map_err(failed)?;
        Ok(structured(json!({"thread_id": thread_id, "closed": true})))
    }
}

/// Official collaboration tool provider.
pub struct AgentToolsPlugin;

impl Default for AgentToolsPlugin {
    fn default() -> Self {
        Self
    }
}

#[async_trait]
impl Plugin for AgentToolsPlugin {
    fn id(&self) -> &str {
        "tools.agents"
    }

    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let provider: Arc<dyn ToolProvider> = Arc::new(StaticToolProvider::new(vec![
            ToolBinding::new(AgentSpawnTool::descriptor(), AgentSpawnTool),
            ToolBinding::new(ThreadCreateTool::descriptor(), ThreadCreateTool),
            ToolBinding::new(ThreadSendTool::descriptor(), ThreadSendTool),
            ToolBinding::new(ThreadAnswerTool::descriptor(), ThreadAnswerTool),
            ToolBinding::new(ThreadWaitTool::descriptor(), ThreadWaitTool),
            ToolBinding::new(ThreadReadTool::descriptor(), ThreadReadTool),
            ToolBinding::new(ThreadListTool::descriptor(), ThreadListTool),
            ToolBinding::new(ThreadCloseTool::descriptor(), ThreadCloseTool),
        ]));
        context.contribute(ToolProviderService(provider.clone()))?;
        context.contribute_extension(provider)
    }
}
