use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_tools_agents::{AgentToolsPlugin, ThreadCreateTool};
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
            let thread_id = request
                .messages
                .iter()
                .rev()
                .filter(|message| message.role == Role::Tool)
                .filter_map(|message| serde_json::from_str::<Value>(&message.content.text()).ok())
                .find_map(|value| {
                    value
                        .get("thread_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| panic!("thread create returns an ID: {value}"));
            if value.get("conversation").is_some() {
                return Ok(ChatResponse::tool_calls(vec![ToolCall::new(
                    "send-call",
                    "thread_send",
                    json!({"thread_id":thread_id,"message":"thread followup"}),
                )]));
            }
            if value.get("threads").is_some() {
                return Ok(ChatResponse::text("parent received thread"));
            }
            if value.get("output").is_none() {
                return Ok(ChatResponse::tool_calls(vec![ToolCall::new(
                    "wait-call",
                    "thread_wait",
                    json!({"thread_id":thread_id}),
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
                json!({"thread_id":thread_id,"limit":20}),
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
            let thread_id = values
                .iter()
                .find_map(|value| value.get("thread_id").and_then(Value::as_str))
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
                    json!({"thread_id":thread_id}),
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
                        "thread_id":thread_id,
                        "interaction_id":interaction_id,
                        "answer":{"answers":answers}
                    }),
                )]));
            }
            return Ok(ChatResponse::tool_calls(vec![ToolCall::new(
                call_id,
                "thread_wait",
                json!({"thread_id":thread_id}),
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
fn thread_tool_schema_cannot_raise_host_default_limits() {
    let defaults = RunLimits::default();
    let schema = compile_schema(&ThreadCreateTool::descriptor().input_schema).unwrap();
    assert!(!schema.is_valid(&json!({
        "task":"too many steps",
        "max_steps":defaults.max_steps + 1
    })));
    assert!(!schema.is_valid(&json!({
        "task":"too many tokens",
        "max_total_tokens":defaults.max_total_tokens + 1
    })));
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
