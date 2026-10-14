//! Official model-callable tools for one-shot delegation and independent threads.
//!
//! The plugin is deliberately a thin protocol adapter. The runtime bridge is
//! resolved from the current [`RunContext`], so applications can replace the
//! default in-process implementation with an authenticated remote coordinator.
use async_trait::async_trait;
use eliteforge_agent_core::{
    AgentCollaborationRuntime, AgentCollaborationService, AgentError, ChildRunResult, ChildRunView,
    Content, Plugin, PluginContext, PluginError, RunContext, RunId, RunLimits, RunRequest,
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
    #[serde(default, rename = "agentCode")]
    agent_code: Option<String>,
    task: String,
    #[serde(default)]
    background: bool,
    #[serde(default)]
    max_total_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateThreadInput {
    #[serde(default, rename = "agentCode")]
    agent_code: Option<String>,
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
    session_id: String,
    message: String,
    #[serde(default)]
    max_steps: Option<usize>,
    #[serde(default)]
    max_total_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerThreadInput {
    session_id: String,
    interaction_id: RunId,
    answer: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitThreadInput {
    session_id: String,
    #[serde(default)]
    run_id: Option<RunId>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadThreadInput {
    session_id: String,
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
    session_id: String,
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

fn agent_code(value: Option<String>) -> Result<Option<String>, ToolError> {
    if value
        .as_ref()
        .is_some_and(|code| code.trim().is_empty() || code.len() > 256 || code.trim() != code)
    {
        return Err(ToolError::Failed(
            "agentCode must be nonempty, at most 256 bytes and have no surrounding whitespace"
                .into(),
        ));
    }
    Ok(value)
}

fn session_id(value: String) -> Result<String, ToolError> {
    if value.trim().is_empty() || value.len() > MAX_THREAD_ID_BYTES {
        return Err(ToolError::Failed(
            "session_id must be nonempty and at most 256 bytes".into(),
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentDetailsInput {
    #[serde(rename = "agentCode")]
    agent_code: String,
}

pub struct AgentGetTool;
impl AgentGetTool {
    pub fn descriptor() -> ToolDescriptor {
        ToolDescriptor::new(
            "agent.get", "agent_get",
            "Read detailed configuration for one authorized Agent from the current directory. Query only; does not start an Agent or change its configuration.",
            json!({"type":"object","additionalProperties":false,"required":["agentCode"],"properties":{
                "agentCode":{"type":"string","minLength":1,"maxLength":256}
            }}),
        ).with_operation_kind(eliteforge_agent_core::ToolOperationKind::Query)
    }
}
#[async_trait]
impl Tool for AgentGetTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        context.check_cancelled().map_err(failed)?;
        let input: AgentDetailsInput = serde_json::from_value(arguments)
            .map_err(|_| ToolError::Failed("invalid agent_get arguments".into()))?;
        let code = agent_code(Some(input.agent_code))?
            .ok_or_else(|| ToolError::Failed("agentCode is required".into()))?;
        Ok(structured(
            runtime(context)?
                .agent_details(&code, context)
                .await
                .map_err(failed)?,
        ))
    }
}

pub struct AgentSpawnTool;

impl AgentSpawnTool {
    pub fn descriptor() -> ToolDescriptor {
        let ceilings = RunLimits::default();
        ToolDescriptor::new(
            "agent.spawn",
            "agent_spawn",
            "Delegate one focused task to a child run. Default (background=false) waits inline for the child result; the child inherits your run's cancellation and deadline. Set background=true when the task can run in parallel: the child runs detached with its own deadline, the tool returns a child_run_id immediately, and you continue other work. Collect a detached result with child_wait when needed; its completion is also notified to you, so do not poll. Step and tool-call limits are resolved by the host from the target Agent configuration; do not set them in the task unless the user explicitly requests a task constraint. Work that must survive the current turn, cross sessions or wait for user decisions belongs in thread_create, not a background spawn.",
            json!({
                "type":"object","additionalProperties":false,"required":["task"],"properties":{
                    "task":{"type":"string","minLength":1,"maxLength":16384},
                    "agentCode":{"type":"string","minLength":1,"maxLength":256,"description":"Target Agent code from the available Agent directory. Omit to use the current Agent. The host resolves configuration and checks authorization."},
                    "background":{"type":"boolean","description":"Run the child detached and return a child_run_id immediately so work can continue in parallel; collect the result with child_wait. Default: false."},
                    "max_total_tokens":{"type":"integer","minimum":1,"maximum":ceilings.max_total_tokens}
                }
            }),
        )
            .with_operation_kind(eliteforge_agent_core::ToolOperationKind::Command)
            .with_timeout_scope(eliteforge_agent_core::ToolTimeoutScope::Run)
    }
}

#[async_trait]
impl Tool for AgentSpawnTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let input: SpawnInput = serde_json::from_value(arguments)
            .map_err(|_| ToolError::Failed("invalid agent_spawn arguments".into()))?;
        let task = task(input.task)?;
        let agent_code = agent_code(input.agent_code)?;
        let runtime = runtime(context)?;
        let mut run = request(task, limits(None, input.max_total_tokens, context)?);
        run.agent_code = agent_code;
        if input.background {
            let summary = runtime
                .spawn_child_detached(run, context)
                .await
                .map_err(failed)?;
            return Ok(structured(
                serde_json::to_value(&summary).expect("child summary is serializable"),
            ));
        }
        let result = runtime.spawn_child(run, context).await.map_err(failed)?;
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
            "Start an independent, durable agent thread for work that must outlive the current turn, keep its own conversation across messages, or wait for user decisions and notifications. Returns immediately; continue other work, then use thread_wait when its result is needed. For same-session parallel delegation use agent_spawn with background=true instead.",
            json!({
                "type":"object","additionalProperties":false,"required":["task"],"properties":{
                    "task":{"type":"string","minLength":1,"maxLength":16384},
                    "agentCode":{"type":"string","minLength":1,"maxLength":256,"description":"Target Agent code from the available Agent directory. Omit to use the current Agent. The host resolves configuration and checks authorization."},
                    "title":{"type":"string","minLength":1,"maxLength":200},
                    "max_steps":{"type":"integer","minimum":1,"maximum":ceilings.max_steps},
                    "max_total_tokens":{"type":"integer","minimum":1,"maximum":ceilings.max_total_tokens}
                }
            }),
        ).with_operation_kind(eliteforge_agent_core::ToolOperationKind::Command)
    }
}

#[async_trait]
impl Tool for ThreadCreateTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let input: CreateThreadInput = serde_json::from_value(arguments)
            .map_err(|_| ToolError::Failed("invalid thread_create arguments".into()))?;
        let task = task(input.task)?;
        let agent_code = agent_code(input.agent_code)?;
        if input
            .title
            .as_ref()
            .is_some_and(|title| title.trim().is_empty())
        {
            return Err(ToolError::Failed("title must not be blank".into()));
        }
        let runtime = runtime(context)?;
        let mut run = request(
            task,
            limits(input.max_steps, input.max_total_tokens, context)?,
        );
        run.agent_code = agent_code;
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
                "type":"object","additionalProperties":false,"required":["session_id","message"],"properties":{
                    "session_id":{"type":"string","minLength":1,"maxLength":256},
                    "message":{"type":"string","minLength":1,"maxLength":16384},
                    "max_steps":{"type":"integer","minimum":1,"maximum":ceilings.max_steps},
                    "max_total_tokens":{"type":"integer","minimum":1,"maximum":ceilings.max_total_tokens}
                }
            }),
        ).with_operation_kind(eliteforge_agent_core::ToolOperationKind::Command)
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
                &session_id(input.session_id)?,
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
                "required":["session_id","interaction_id","answer"],"properties":{
                    "session_id":{"type":"string","minLength":1,"maxLength":256},
                    "interaction_id":{"type":"string","format":"uuid"},
                    "answer":{}
                }
            }),
        )
        .with_operation_kind(eliteforge_agent_core::ToolOperationKind::Command)
    }
}

#[async_trait]
impl Tool for ThreadAnswerTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let input: AnswerThreadInput = serde_json::from_value(arguments)
            .map_err(|_| ToolError::Failed("invalid thread_answer arguments".into()))?;
        let summary = runtime(context)?
            .answer_thread(
                &session_id(input.session_id)?,
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
                "type":"object","additionalProperties":false,"required":["session_id"],"properties":{
                    "session_id":{"type":"string","minLength":1,"maxLength":256},
                    "run_id":{"type":"string","format":"uuid"}
                }
            }),
        )
            .with_operation_kind(eliteforge_agent_core::ToolOperationKind::Query)
            .with_timeout_scope(eliteforge_agent_core::ToolTimeoutScope::Run)
    }
}

#[async_trait]
impl Tool for ThreadWaitTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let input: WaitThreadInput = serde_json::from_value(arguments)
            .map_err(|_| ToolError::Failed("invalid thread_wait arguments".into()))?;
        let result = runtime(context)?
            .wait_thread(&session_id(input.session_id)?, input.run_id, context)
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
                "type":"object","additionalProperties":false,"required":["session_id"],"properties":{
                    "session_id":{"type":"string","minLength":1,"maxLength":256},
                    "limit":{"type":"integer","minimum":1,"maximum":100}
                }
            }),
        ).with_operation_kind(eliteforge_agent_core::ToolOperationKind::Query)
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
            .read_thread(&session_id(input.session_id)?, input.limit)
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
        .with_operation_kind(eliteforge_agent_core::ToolOperationKind::Query)
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
                "type":"object","additionalProperties":false,"required":["session_id"],"properties":{
                    "session_id":{"type":"string","minLength":1,"maxLength":256}
                }
            }),
        ).with_operation_kind(eliteforge_agent_core::ToolOperationKind::Command)
    }
}

#[async_trait]
impl Tool for ThreadCloseTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let input: CloseThreadInput = serde_json::from_value(arguments)
            .map_err(|_| ToolError::Failed("invalid thread_close arguments".into()))?;
        let session_id = session_id(input.session_id)?;
        runtime(context)?
            .close_thread(&session_id, context)
            .await
            .map_err(failed)?;
        Ok(structured(
            json!({"session_id": session_id, "closed": true}),
        ))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitChildInput {
    child_run_id: RunId,
}

pub struct ChildWaitTool;

impl ChildWaitTool {
    pub fn descriptor() -> ToolDescriptor {
        ToolDescriptor::new(
            "child.wait",
            "child_wait",
            "Wait for one detached child run started by agent_spawn with background=true and return its final result. Completion is normally notified to you, so call this only when you need the result to proceed.",
            json!({
                "type":"object","additionalProperties":false,"required":["child_run_id"],"properties":{
                    "child_run_id":{"type":"string","format":"uuid"}
                }
            }),
        )
        .with_operation_kind(eliteforge_agent_core::ToolOperationKind::Query)
        .with_timeout_scope(eliteforge_agent_core::ToolTimeoutScope::Run)
    }
}

#[async_trait]
impl Tool for ChildWaitTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let input: WaitChildInput = serde_json::from_value(arguments)
            .map_err(|_| ToolError::Failed("invalid child_wait arguments".into()))?;
        let result = runtime(context)?
            .wait_child(input.child_run_id, context)
            .await
            .map_err(failed)?;
        Ok(spawn_result(result))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SendChildInput {
    child_run_id: RunId,
    message: String,
}

pub struct ChildSendTool;

impl ChildSendTool {
    pub fn descriptor() -> ToolDescriptor {
        ToolDescriptor::new(
            "child.send",
            "child_send",
            "Queue one steering message for a running detached child run. The child receives it at its next steering boundary; no result is returned here.",
            json!({
                "type":"object","additionalProperties":false,"required":["child_run_id","message"],"properties":{
                    "child_run_id":{"type":"string","format":"uuid"},
                    "message":{"type":"string","minLength":1,"maxLength":16384}
                }
            }),
        )
        .with_operation_kind(eliteforge_agent_core::ToolOperationKind::Command)
    }
}

#[async_trait]
impl Tool for ChildSendTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let input: SendChildInput = serde_json::from_value(arguments)
            .map_err(|_| ToolError::Failed("invalid child_send arguments".into()))?;
        let summary = runtime(context)?
            .send_child(input.child_run_id, task(input.message)?, context)
            .await
            .map_err(failed)?;
        Ok(structured(
            serde_json::to_value(&summary).expect("child summary is serializable"),
        ))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerChildInput {
    child_run_id: RunId,
    interaction_id: RunId,
    answer: Value,
}

pub struct ChildAnswerTool;

impl ChildAnswerTool {
    pub fn descriptor() -> ToolDescriptor {
        ToolDescriptor::new(
            "child.answer",
            "child_answer",
            "Answer a pending interaction of a detached child run and resume it. The child keeps its child_run_id and conversation.",
            json!({
                "type":"object","additionalProperties":false,
                "required":["child_run_id","interaction_id","answer"],"properties":{
                    "child_run_id":{"type":"string","format":"uuid"},
                    "interaction_id":{"type":"string","format":"uuid"},
                    "answer":{}
                }
            }),
        )
        .with_operation_kind(eliteforge_agent_core::ToolOperationKind::Command)
    }
}

#[async_trait]
impl Tool for ChildAnswerTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let input: AnswerChildInput = serde_json::from_value(arguments)
            .map_err(|_| ToolError::Failed("invalid child_answer arguments".into()))?;
        let summary = runtime(context)?
            .answer_child(
                input.child_run_id,
                input.interaction_id,
                input.answer,
                context,
            )
            .await
            .map_err(failed)?;
        Ok(structured(
            serde_json::to_value(&summary).expect("child summary is serializable"),
        ))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelChildInput {
    child_run_id: RunId,
}

pub struct ChildCancelTool;

impl ChildCancelTool {
    pub fn descriptor() -> ToolDescriptor {
        ToolDescriptor::new(
            "child.cancel",
            "child_cancel",
            "Request cancellation of one detached child run. Collect its terminal status with child_wait afterwards.",
            json!({
                "type":"object","additionalProperties":false,"required":["child_run_id"],"properties":{
                    "child_run_id":{"type":"string","format":"uuid"}
                }
            }),
        )
        .with_operation_kind(eliteforge_agent_core::ToolOperationKind::Command)
    }
}

#[async_trait]
impl Tool for ChildCancelTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let input: CancelChildInput = serde_json::from_value(arguments)
            .map_err(|_| ToolError::Failed("invalid child_cancel arguments".into()))?;
        let summary = runtime(context)?
            .cancel_child(input.child_run_id, context)
            .await
            .map_err(failed)?;
        Ok(structured(
            serde_json::to_value(&summary).expect("child summary is serializable"),
        ))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadChildInput {
    child_run_id: RunId,
    #[serde(default = "default_read_limit")]
    limit: usize,
}

pub struct ChildReadTool;

impl ChildReadTool {
    pub fn descriptor() -> ToolDescriptor {
        ToolDescriptor::new(
            "child.read",
            "child_read",
            "Read the summary and recent messages of one detached child run.",
            json!({
                "type":"object","additionalProperties":false,"required":["child_run_id"],"properties":{
                    "child_run_id":{"type":"string","format":"uuid"},
                    "limit":{"type":"integer","minimum":1,"maximum":100}
                }
            }),
        )
        .with_operation_kind(eliteforge_agent_core::ToolOperationKind::Query)
    }
}

#[async_trait]
impl Tool for ChildReadTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let input: ReadChildInput = serde_json::from_value(arguments)
            .map_err(|_| ToolError::Failed("invalid child_read arguments".into()))?;
        if input.limit == 0 || input.limit > MAX_READ_MESSAGES {
            return Err(ToolError::Failed("limit must be between 1 and 100".into()));
        }
        let view: ChildRunView = runtime(context)?
            .read_child(input.child_run_id, input.limit)
            .await
            .map_err(failed)?;
        Ok(structured(
            serde_json::to_value(&view).expect("child view is serializable"),
        ))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListChildrenInput {
    #[serde(default = "default_list_limit")]
    limit: usize,
}

pub struct ChildListTool;

impl ChildListTool {
    pub fn descriptor() -> ToolDescriptor {
        ToolDescriptor::new(
            "child.list",
            "child_list",
            "List detached child runs started by the current agent identity that are still addressable, including finished ones whose result has not been collected.",
            json!({
                "type":"object","additionalProperties":false,"properties":{
                    "limit":{"type":"integer","minimum":1,"maximum":100}
                }
            }),
        )
        .with_operation_kind(eliteforge_agent_core::ToolOperationKind::Query)
    }
}

#[async_trait]
impl Tool for ChildListTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let input: ListChildrenInput = serde_json::from_value(arguments)
            .map_err(|_| ToolError::Failed("invalid child_list arguments".into()))?;
        if input.limit == 0 || input.limit > MAX_READ_MESSAGES {
            return Err(ToolError::Failed("limit must be between 1 and 100".into()));
        }
        let mut children = runtime(context)?
            .list_children(context)
            .await
            .map_err(failed)?;
        children.truncate(input.limit);
        Ok(structured(json!({"children": children})))
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
            ToolBinding::new(AgentGetTool::descriptor(), AgentGetTool),
            ToolBinding::new(AgentSpawnTool::descriptor(), AgentSpawnTool),
            ToolBinding::new(ChildWaitTool::descriptor(), ChildWaitTool),
            ToolBinding::new(ChildSendTool::descriptor(), ChildSendTool),
            ToolBinding::new(ChildAnswerTool::descriptor(), ChildAnswerTool),
            ToolBinding::new(ChildCancelTool::descriptor(), ChildCancelTool),
            ToolBinding::new(ChildReadTool::descriptor(), ChildReadTool),
            ToolBinding::new(ChildListTool::descriptor(), ChildListTool),
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

#[cfg(test)]
mod named_agent_tests {
    use super::*;

    #[test]
    fn target_code_is_optional_and_preserved_exactly() {
        let value = json!({"task":"review","agentCode":"database-reviewer"});
        let spawn: SpawnInput = serde_json::from_value(value.clone()).unwrap();
        let thread: CreateThreadInput = serde_json::from_value(value).unwrap();
        assert_eq!(
            agent_code(spawn.agent_code).unwrap().as_deref(),
            Some("database-reviewer")
        );
        assert_eq!(
            agent_code(thread.agent_code).unwrap().as_deref(),
            Some("database-reviewer")
        );
        let legacy: SpawnInput = serde_json::from_value(json!({"task":"review"})).unwrap();
        assert!(legacy.agent_code.is_none());
        for value in [
            "".into(),
            " ".into(),
            " reviewer".into(),
            "reviewer ".into(),
            "x".repeat(257),
        ] {
            assert!(agent_code(Some(value)).is_err());
        }
        let mut run = RunRequest::new("test", "review");
        run.agent_code = Some("reviewer".into());
        let encoded = serde_json::to_value(&run).unwrap();
        assert_eq!(encoded["agentCode"], "reviewer");
        assert_eq!(
            serde_json::from_value::<RunRequest>(encoded)
                .unwrap()
                .agent_code,
            run.agent_code
        );
    }
}
