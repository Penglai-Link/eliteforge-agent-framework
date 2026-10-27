use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_tools_agents::{
    AgentSpawnTool, AgentToolsPlugin, ThreadAnswerTool, ThreadCloseTool, ThreadCreateTool,
    ThreadReadTool, ThreadSendTool, ThreadWaitTool,
};
use eliteforge_agent_official_plugin_tools_human::HumanInteractionPlugin;
use eliteforge_agent_test_support::TestBuilder;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct CollaborationEvents(Arc<Mutex<Vec<CollaborationEvent>>>);

#[async_trait]
impl EventSink for CollaborationEvents {
    async fn on_event(&self, _: &RunContext, event: &AgentEvent) {
        if let AgentEvent::Collaboration(event) = event {
            self.0.lock().unwrap().push((**event).clone());
        }
    }
}

#[derive(Clone, Default)]
struct CollaborationModel;

fn last_tool_value(request: &ChatRequest) -> Option<Value> {
    request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Tool)
        .and_then(|message| serde_json::from_str(&message.content.text()).ok())
}

fn tool_values(request: &ChatRequest) -> Vec<Value> {
    request
        .messages
        .iter()
        .rev()
        .filter(|message| message.role == Role::Tool)
        .filter_map(|message| serde_json::from_str(&message.content.text()).ok())
        .collect()
}

fn any_child_run_id(values: &[Value]) -> Option<String> {
    values.iter().find_map(|value| {
        value
            .get("child_run_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
    })
}

/// Scripted flows for detached child runs (`agent_spawn background=true` plus
/// the child_* tool suite).
fn detached_script(request: &ChatRequest, input: &str, has_tool: bool) -> Option<ChatResponse> {
    let spawn = |task: &str| {
        ChatResponse::tool_calls(vec![ToolCall::new(
            "detached-spawn",
            "agent_spawn",
            json!({"task":task, "background": true}),
        )])
    };
    let sent_child_send = request.messages.iter().any(|message| {
        message.role == Role::Assistant
            && message
                .tool_calls
                .iter()
                .any(|call| call.call_name == "child_send")
    });
    match input {
        "use detached child" => {
            if !has_tool {
                return Some(spawn("child task"));
            }
            let values = tool_values(request);
            let latest = values.first().expect("tool output is JSON");
            if let Some(child_run_id) = latest.get("child_run_id").and_then(Value::as_str) {
                return Some(ChatResponse::tool_calls(vec![ToolCall::new(
                    "detached-wait",
                    "child_wait",
                    json!({"child_run_id": child_run_id}),
                )]));
            }
            assert_eq!(
                latest.get("output").and_then(Value::as_str),
                Some("child result")
            );
            Some(ChatResponse::text("parent received detached child"))
        }
        "cancel detached child" => {
            if !has_tool {
                return Some(spawn("slow child task"));
            }
            let values = tool_values(request);
            let child_run_id = any_child_run_id(&values).expect("spawn returns a child run ID");
            let sent_child_cancel = request.messages.iter().any(|message| {
                message.role == Role::Assistant
                    && message
                        .tool_calls
                        .iter()
                        .any(|call| call.call_name == "child_cancel")
            });
            let sent_child_wait = request.messages.iter().any(|message| {
                message.role == Role::Assistant
                    && message
                        .tool_calls
                        .iter()
                        .any(|call| call.call_name == "child_wait")
            });
            if !sent_child_cancel {
                return Some(ChatResponse::tool_calls(vec![ToolCall::new(
                    "detached-cancel",
                    "child_cancel",
                    json!({"child_run_id": child_run_id}),
                )]));
            }
            if !sent_child_wait {
                return Some(ChatResponse::tool_calls(vec![ToolCall::new(
                    "detached-cancel-wait",
                    "child_wait",
                    json!({"child_run_id": child_run_id}),
                )]));
            }
            let cancelled_by_result = values.iter().any(|value| {
                value.get("run_id").is_some()
                    && value.get("status").and_then(Value::as_str) == Some("cancelled")
            });
            let cancelled_by_error = request
                .messages
                .iter()
                .rev()
                .find(|message| message.role == Role::Tool)
                .map(|message| message.content.text().to_lowercase())
                .is_some_and(|text| text.contains("cancel"));
            assert!(
                cancelled_by_result || cancelled_by_error,
                "cancelled child surfaces a terminal status, got {values:?}"
            );
            Some(ChatResponse::text("parent cancelled detached child"))
        }
        "spawn orphan child" => {
            if !has_tool {
                return Some(spawn("child task"));
            }
            Some(ChatResponse::text("orphan spawned"))
        }
        "collect orphan child" => {
            let values = tool_values(request);
            let child_run_id = any_child_run_id(&values).expect("history keeps the child run ID");
            let latest = values.first().expect("tool output is JSON");
            if latest.get("child_run_id").is_some() {
                return Some(ChatResponse::tool_calls(vec![ToolCall::new(
                    "orphan-wait",
                    "child_wait",
                    json!({"child_run_id": child_run_id}),
                )]));
            }
            assert_eq!(
                latest.get("output").and_then(Value::as_str),
                Some("child result")
            );
            Some(ChatResponse::text("orphan collected"))
        }
        "inspect detached child" => {
            if !has_tool {
                return Some(spawn("child task"));
            }
            let values = tool_values(request);
            let child_run_id = any_child_run_id(&values).expect("spawn returns a child run ID");
            let latest = values.first().expect("tool output is JSON");
            if latest.get("conversation").is_some() {
                return Some(ChatResponse::tool_calls(vec![ToolCall::new(
                    "inspect-list",
                    "child_list",
                    json!({"limit": 20}),
                )]));
            }
            if latest.get("children").is_some() {
                return Some(ChatResponse::tool_calls(vec![ToolCall::new(
                    "inspect-wait",
                    "child_wait",
                    json!({"child_run_id": child_run_id}),
                )]));
            }
            if latest.get("output").is_none() {
                return Some(ChatResponse::tool_calls(vec![ToolCall::new(
                    "inspect-read",
                    "child_read",
                    json!({"child_run_id": child_run_id, "limit": 20}),
                )]));
            }
            assert_eq!(
                latest.get("output").and_then(Value::as_str),
                Some("child result")
            );
            Some(ChatResponse::text("parent inspected detached child"))
        }
        "steer finished child" => {
            if !has_tool {
                return Some(spawn("child task"));
            }
            let values = tool_values(request);
            let child_run_id = any_child_run_id(&values).expect("spawn returns a child run ID");
            if !sent_child_send {
                return Some(ChatResponse::tool_calls(vec![ToolCall::new(
                    "steer-send",
                    "child_send",
                    json!({"child_run_id": child_run_id, "message": "reconsider"}),
                )]));
            }
            Some(ChatResponse::text("parent saw steering rejected"))
        }
        _ => None,
    }
}

#[async_trait]
impl ChatModel for CollaborationModel {
    async fn generate(
        &self,
        _: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        let input = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User)
            .map(|message| message.content.text())
            .unwrap_or_default();
        let has_tool = request
            .messages
            .iter()
            .any(|message| message.role == Role::Tool);
        let interactive_thread = request.messages.iter().any(|message| {
            message.role == Role::User && message.content.text() == "interactive thread task"
        });
        if interactive_thread {
            return Ok(if has_tool {
                ChatResponse::text("interactive thread result")
            } else {
                ChatResponse::tool_calls(vec![ToolCall::new(
                    "ask-thread-user",
                    "ask_user",
                    json!({
                        "intro":"Choose a deployment target",
                        "questions":[{
                            "question":"Where should this run?",
                            "options":[{"label":"staging"},{"label":"production"}]
                        }]
                    }),
                )])
            });
        }
        if input == "child task" || input == "thread task" || input == "thread followup" {
            return Ok(ChatResponse::text(match input.as_str() {
                "child task" => "child result",
                "thread task" => "thread result",
                _ => "thread followup result",
            }));
        }
        if input == "slow child task" {
            // Give the parent time to cancel before the child finishes.
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            return Ok(ChatResponse::text("child result"));
        }
        if let Some(command) = detached_script(&request, &input, has_tool) {
            return Ok(command);
        }
        if input == "named child" || input == "named thread" {
            return Ok(if !has_tool {
                ChatResponse::tool_calls(vec![ToolCall::new(
                    "named-call",
                    if input == "named child" {
                        "agent_spawn"
                    } else {
                        "thread_create"
                    },
                    json!({"task":"child task","agentCode":"reviewer"}),
                )])
            } else {
                let error = request
                    .messages
                    .iter()
                    .rev()
                    .find(|message| message.role == Role::Tool)
                    .unwrap()
                    .content
                    .text();
                assert!(error.contains("resolve and authorize agentCode"), "{error}");
                ChatResponse::text("named selection rejected")
            });
        }
        if input == "use child" {
            return Ok(if !has_tool {
                ChatResponse::tool_calls(vec![ToolCall::new(
                    "child-call",
                    "agent_spawn",
                    json!({"task":"child task"}),
                )])
            } else {
                let value = last_tool_value(&request).expect("spawn output is JSON");
                assert!(value.get("content").is_some());
                assert!(value.get("interactions").is_some_and(Value::is_array));
                ChatResponse::text("parent received child")
            });
        }
        if input == "amplify thread" {
            return Ok(if !has_tool {
                ChatResponse::tool_calls(vec![ToolCall::new(
                    "amplify-create",
                    "thread_create",
                    json!({"task":"too much work","max_total_tokens":100_001}),
                )])
            } else {
                let error = request
                    .messages
                    .iter()
                    .rev()
                    .find(|message| message.role == Role::Tool)
                    .map(|message| message.content.text())
                    .unwrap_or_default();
                assert!(error.contains("between 1 and 100000"));
                ChatResponse::text("thread budget rejected")
            });
        }
        if input == "use thread" {
            if !has_tool {
                return Ok(ChatResponse::tool_calls(vec![ToolCall::new(
                    "create-call",
                    "thread_create",
                    json!({"task":"thread task","title":"worker"}),
                )]));
            }
            let value = last_tool_value(&request).expect("tool output is JSON");
            let value = value
                .get("structured")
                .filter(|value| value.is_object())
                .unwrap_or(&value);
            let session_id = request
                .messages
                .iter()
                .rev()
                .filter(|message| message.role == Role::Tool)
                .filter_map(|message| serde_json::from_str::<Value>(&message.content.text()).ok())
                .find_map(|value| {
                    value
                        .get("session_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| panic!("thread create returns an ID: {value}"));
            if value.get("conversation").is_some() {
                return Ok(ChatResponse::tool_calls(vec![ToolCall::new(
                    "send-call",
                    "thread_send",
                    json!({"session_id":session_id,"message":"thread followup"}),
                )]));
            }
            if value.get("threads").is_some() {
                return Ok(ChatResponse::text("parent received thread"));
            }
            if value.get("output").is_none() {
                return Ok(ChatResponse::tool_calls(vec![ToolCall::new(
                    "wait-call",
                    "thread_wait",
                    json!({"session_id":session_id}),
                )]));
            }
            if value.get("output").and_then(Value::as_str) == Some("thread followup result") {
                return Ok(ChatResponse::tool_calls(vec![ToolCall::new(
                    "list-call",
                    "thread_list",
                    json!({"limit":20}),
                )]));
            }
            return Ok(ChatResponse::tool_calls(vec![ToolCall::new(
                "read-call",
                "thread_read",
                json!({"session_id":session_id,"limit":20}),
            )]));
        }
        if input == "use interactive thread" {
            if !has_tool {
                return Ok(ChatResponse::tool_calls(vec![ToolCall::new(
                    "interactive-create",
                    "thread_create",
                    json!({"task":"interactive thread task","title":"interactive worker"}),
                )]));
            }
            let values = request
                .messages
                .iter()
                .rev()
                .filter(|message| message.role == Role::Tool)
                .filter_map(|message| serde_json::from_str::<Value>(&message.content.text()).ok())
                .collect::<Vec<_>>();
            let latest = &values[0];
            let session_id = values
                .iter()
                .find_map(|value| value.get("session_id").and_then(Value::as_str))
                .expect("thread create returns an ID");
            let call_id = format!("interactive-call-{}", values.len());
            if let Some(threads) = latest.get("threads").and_then(Value::as_array) {
                assert!(threads.is_empty(), "closed thread must release its slot");
                return Ok(ChatResponse::text("parent resumed and closed thread"));
            }
            if latest.get("closed").and_then(Value::as_bool) == Some(true) {
                return Ok(ChatResponse::tool_calls(vec![ToolCall::new(
                    call_id,
                    "thread_list",
                    json!({"limit":20}),
                )]));
            }
            if latest.get("output").and_then(Value::as_str) == Some("interactive thread result") {
                return Ok(ChatResponse::tool_calls(vec![ToolCall::new(
                    call_id,
                    "thread_close",
                    json!({"session_id":session_id}),
                )]));
            }
            if let Some(interaction) = latest
                .get("interactions")
                .and_then(Value::as_array)
                .and_then(|interactions| interactions.first())
            {
                let interaction_id = interaction["id"].as_str().expect("interaction has an ID");
                let question = &interaction["presentation"]["questions"][0];
                let question_id = question["id"].as_str().expect("question has an ID");
                let option_id = question["options"][0]["id"]
                    .as_str()
                    .expect("option has an ID");
                let mut answers = serde_json::Map::new();
                answers.insert(question_id.into(), json!({"option_id":option_id}));
                return Ok(ChatResponse::tool_calls(vec![ToolCall::new(
                    call_id,
                    "thread_answer",
                    json!({
                        "session_id":session_id,
                        "interaction_id":interaction_id,
                        "answer":{"answers":answers}
                    }),
                )]));
            }
            return Ok(ChatResponse::tool_calls(vec![ToolCall::new(
                call_id,
                "thread_wait",
                json!({"session_id":session_id}),
            )]));
        }
        Err(ModelError::Fatal("unexpected scripted request".into()))
    }
}

async fn agent() -> Agent {
    TestBuilder::default()
        .plugin(ChatModelPlugin::new(CollaborationModel))
        .plugin(AgentToolsPlugin)
        .build()
        .await
        .unwrap()
}

#[test]
fn spawn_tool_uses_host_agent_limits_instead_of_model_step_overrides() {
    let schema = compile_schema(&AgentSpawnTool::descriptor().input_schema).unwrap();
    assert!(schema.is_valid(&json!({"task":"review", "agentCode":"reviewer"})));
    for field in ["max_steps", "max_tool_calls"] {
        let mut input = json!({"task":"review"});
        input[field] = json!(1);
        assert!(!schema.is_valid(&input));
    }
}

#[test]
fn thread_tool_schema_keeps_step_limits_without_an_implicit_token_ceiling() {
    let defaults = RunLimits::default();
    let schema = compile_schema(&ThreadCreateTool::descriptor().input_schema).unwrap();
    assert!(!schema.is_valid(&json!({
        "task":"too many steps",
        "max_steps":defaults.max_steps + 1
    })));
    assert_eq!(defaults.max_total_tokens, RunLimits::UNLIMITED_TOKENS);
    for tokens in [2_000_000, RunLimits::UNLIMITED_TOKENS] {
        assert!(schema.is_valid(&json!({"task":"explicit quota", "max_total_tokens":tokens})));
    }
    assert!(!schema.is_valid(&json!({"task":"invalid quota", "max_total_tokens":0})));
}

#[test]
fn thread_tools_address_native_sessions_without_an_alias_id() {
    use eliteforge_agent_core::compile_schema;
    for descriptor in [
        ThreadSendTool::descriptor(),
        ThreadReadTool::descriptor(),
        ThreadWaitTool::descriptor(),
        ThreadAnswerTool::descriptor(),
        ThreadCloseTool::descriptor(),
    ] {
        assert!(
            descriptor.input_schema["properties"]
                .get("session_id")
                .is_some()
        );
        assert!(
            descriptor.input_schema["properties"]
                .get("thread_id")
                .is_none()
        );
        let schema = compile_schema(&descriptor.input_schema).unwrap();
        assert!(!schema.is_valid(&json!({"thread_id":"alias"})));
    }
    let summary = serde_json::to_value(ThreadSummary {
        session_id: "native-session".into(),
        title: None,
        status: ThreadStatus::Completed,
        latest_run_id: None,
    })
    .unwrap();
    assert_eq!(summary["session_id"], "native-session");
    assert!(summary.get("thread_id").is_none());
}

#[tokio::test]
async fn thread_tool_cannot_raise_the_current_run_limits() {
    let agent = agent().await;
    let mut request = RunRequest::new("parent-budget", "amplify thread");
    request.limits.max_total_tokens = 100_000;
    let result = agent.run(request).await.unwrap();
    assert_eq!(result.output, "thread budget rejected");
    assert!(agent.framework().close().await.is_success());
}

#[tokio::test]
async fn agent_spawn_tool_runs_a_real_child_and_returns_its_result() {
    let agent = agent().await;
    let result = agent
        .run(RunRequest::new("parent-child", "use child"))
        .await
        .unwrap();
    assert_eq!(result.output, "parent received child");
    assert!(agent.framework().close().await.is_success());
}

#[tokio::test]
async fn background_spawn_returns_a_child_run_and_wait_collects_it() {
    let agent = agent().await;
    let result = agent
        .run(RunRequest::new("parent-detached", "use detached child"))
        .await
        .unwrap();
    assert_eq!(result.output, "parent received detached child");
    assert!(agent.framework().close().await.is_success());
}

#[tokio::test]
async fn detached_child_survives_its_parent_run_and_collects_later() {
    let agent = agent().await;
    let session = "orphan-session";
    let spawned = agent
        .run(RunRequest::new(session, "spawn orphan child"))
        .await
        .unwrap();
    assert_eq!(spawned.output, "orphan spawned");
    let collected = agent
        .run(RunRequest::new(session, "collect orphan child"))
        .await
        .unwrap();
    assert_eq!(collected.output, "orphan collected");
    assert!(agent.framework().close().await.is_success());
}

#[tokio::test]
async fn child_cancel_stops_a_detached_child_run() {
    let agent = agent().await;
    let result = agent
        .run(RunRequest::new("parent-cancel", "cancel detached child"))
        .await
        .unwrap();
    assert_eq!(result.output, "parent cancelled detached child");
    assert!(agent.framework().close().await.is_success());
}

#[tokio::test]
async fn child_read_list_and_wait_expose_detached_lifecycle() {
    let agent = agent().await;
    let result = agent
        .run(RunRequest::new("parent-inspect", "inspect detached child"))
        .await
        .unwrap();
    assert_eq!(result.output, "parent inspected detached child");
    assert!(agent.framework().close().await.is_success());
}

#[tokio::test]
async fn child_send_rejects_a_finished_child_run() {
    let agent = agent().await;
    let result = agent
        .run(RunRequest::new("parent-steer", "steer finished child"))
        .await
        .unwrap();
    assert_eq!(result.output, "parent saw steering rejected");
    assert!(agent.framework().close().await.is_success());
}

#[tokio::test]
async fn thread_tools_create_wait_and_read_an_independent_conversation() {
    let agent = agent().await;
    let result = agent
        .run(RunRequest::new("parent-thread", "use thread"))
        .await
        .unwrap();
    assert_eq!(result.output, "parent received thread");
    assert!(agent.framework().close().await.is_success());
}

#[tokio::test]
async fn thread_answer_resumes_a_persisted_interaction_and_close_releases_it() {
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(CollaborationModel))
        .plugin(HumanInteractionPlugin)
        .plugin(AgentToolsPlugin)
        .build()
        .await
        .unwrap();
    let result = agent
        .run(RunRequest::new(
            "parent-interactive-thread",
            "use interactive thread",
        ))
        .await
        .unwrap();
    assert_eq!(result.output, "parent resumed and closed thread");
    assert!(agent.framework().close().await.is_success());
}

#[tokio::test]
async fn collaboration_runtime_publishes_child_and_thread_lifecycle_events() {
    let events = CollaborationEvents::default();
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(CollaborationModel))
        .plugin(HumanInteractionPlugin)
        .plugin(AgentToolsPlugin)
        .plugin(EventPlugin::new("collaboration.events", events.clone()))
        .build()
        .await
        .unwrap();
    assert_eq!(
        agent
            .run(RunRequest::new("events-child", "use child"))
            .await
            .unwrap()
            .output,
        "parent received child"
    );
    assert_eq!(
        agent
            .run(RunRequest::new("events-thread", "use interactive thread",))
            .await
            .unwrap()
            .output,
        "parent resumed and closed thread"
    );

    let events = events.0.lock().unwrap().clone();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, CollaborationEvent::ChildRunStarted { .. }))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                CollaborationEvent::ChildRunFinished {
                    status: RunStatus::Completed,
                    ..
                }
            ))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, CollaborationEvent::ThreadCreated { .. }))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                CollaborationEvent::ThreadRunStarted {
                    cause: ThreadRunCause::Created,
                    interaction_id: None,
                    ..
                }
            ))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                CollaborationEvent::ThreadRunStarted {
                    cause: ThreadRunCause::InteractionResponse,
                    interaction_id: Some(_),
                    ..
                }
            ))
            .count(),
        1
    );
    assert!(events.iter().any(|event| matches!(event,
        CollaborationEvent::ThreadRunFinished {
            status: ThreadStatus::AwaitingInput,
            interactions,
            ..
        } if interactions.len() == 1)));
    assert!(events.iter().any(|event| matches!(event,
        CollaborationEvent::ThreadRunFinished {
            status: ThreadStatus::Completed,
            interactions,
            ..
        } if interactions.is_empty())));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, CollaborationEvent::ThreadClosed { .. }))
            .count(),
        1
    );
    assert!(agent.framework().close().await.is_success());
}

#[tokio::test]
async fn unresolved_named_agents_never_fall_back_to_the_current_agent() {
    let agent = agent().await;
    for input in ["named child", "named thread"] {
        let result = agent.run(RunRequest::new(input, input)).await.unwrap();
        assert_eq!(result.output, "named selection rejected");
    }
    let mut request = RunRequest::new("direct-named", "child task");
    request.agent_code = Some("reviewer".into());
    assert!(agent.run(request).await.is_err());
    assert!(agent.framework().close().await.is_success());
}

#[test]
fn delegation_schema_carries_a_directory_independent_of_task_text() {
    for descriptor in [AgentSpawnTool::descriptor(), ThreadCreateTool::descriptor()] {
        let schema = compile_schema(&descriptor.input_schema).unwrap();
        assert!(schema.is_valid(&json!({"task":"small task", "workingDirectory":"tasks/worker"})));
        assert!(!schema.is_valid(&json!({"task":"small task", "workingDirectory":""})));
    }
    let mut request = RunRequest::new("session", "small task");
    request.working_directory = Some("tasks/worker".into());
    let decoded: RunRequest =
        serde_json::from_value(serde_json::to_value(&request).unwrap()).unwrap();
    assert_eq!(decoded.working_directory, request.working_directory);
}
