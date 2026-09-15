use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_state_sqlite::*;
use eliteforge_agent_test_support::*;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<ChatRequest>>>);
#[async_trait]
impl ChatModel for Capture {
    async fn generate(
        &self,
        _model_id: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        self.0.lock().unwrap().push(request);
        Ok(ChatResponse::text("done"))
    }
}
#[derive(Clone, Default)]
struct Source(Arc<Mutex<Vec<(ConversationKey, String)>>>);
#[async_trait]
impl ContextContributor for Source {
    fn block_id(&self) -> &str {
        "lookup"
    }
    async fn contribute(&self, input: &ContextInput<'_>) -> Result<ContextBlock, AgentError> {
        self.0
            .lock()
            .unwrap()
            .push((input.history.key.clone(), input.history.generation.clone()));
        Ok(ContextBlock::data("lookup", "retrieved facts"))
    }
}
#[tokio::test]
async fn react_workflow_and_dag_share_explicit_layout_and_isolate_node_snapshots() {
    for mode in ["react", "workflow", "dag"] {
        let directory = tempfile::tempdir().unwrap();
        let state = Arc::new(
            SqliteStateStore::open(directory.path().join("state.db"), SqliteOptions::default())
                .await
                .unwrap(),
        );
        let capture = Capture::default();
        let source = Source::default();
        let mut builder = TestBuilder::default()
            .plugin(ChatModelPlugin::new(capture.clone()))
            .plugin(ContextPlugin::new(
                "system",
                FixedContext::new("fixed instructions"),
            ))
            .plugin(ContextContributorPlugin::new(
                "lookup-plugin",
                source.clone(),
            ))
            .plugin(ContextLayoutPlugin(ContextLayout(vec![
                ContextBlockConfig::new(INSTRUCTIONS_BLOCK, BlockBudget::tokens(1000)),
                ContextBlockConfig::new("lookup", BlockBudget::tokens(2000)),
                ContextBlockConfig::new(HISTORY_BLOCK, BlockBudget::tokens(10_000))
                    .reduce_with(RecentConversation),
            ])));
        if mode == "workflow" {
            builder = builder
                .plugin(StateStorePlugin(state.clone()))
                .plugin(AgentLoopPlugin::new(
                    WorkflowLoop::new(WorkflowDefinition {
                        id: "chain".into(),
                        version: 1,
                        stages: vec![WorkflowStage::new("one"), WorkflowStage::new("two")],
                    })
                    .unwrap(),
                ));
        }
        if mode == "dag" {
            builder = builder
                .plugin(StateStorePlugin(state.clone()))
                .plugin(AgentLoopPlugin::new(
                    DagWorkflowLoop::new(DagWorkflowDefinition {
                        id: "graph".into(),
                        version: 1,
                        nodes: vec![
                            DagNode::new(
                                "a/b",
                                "framework.react",
                                serde_json::to_value(WorkflowStage::new("c")).unwrap(),
                            ),
                            DagNode::new(
                                "a",
                                "framework.react",
                                serde_json::to_value(WorkflowStage::new("b/c")).unwrap(),
                            ),
                        ],
                        edges: vec![],
                        max_parallel_nodes: 2,
                    })
                    .unwrap(),
                ));
        }
        let agent = builder.build().await.unwrap();
        agent
            .run(RunRequest::new("session", "question"))
            .await
            .unwrap();
        if mode != "react" {
            let checkpoint = state.latest("session").await.unwrap().unwrap();
            let mut request = RunRequest::new("session", "ignored on completed replay");
            request.resume = Some(ResumeRequest {
                run_id: checkpoint.run_id,
                ..Default::default()
            });
            agent.run(request).await.unwrap();
        }
        {
            let requests = capture.0.lock().unwrap();
            assert_eq!(requests.len(), if mode == "react" { 1 } else { 2 });
            for request in requests.iter() {
                assert_eq!(request.messages[0].content.text(), "fixed instructions");
                assert_eq!(request.messages[1].content.text(), "retrieved facts");
                assert_eq!(request.messages[1].role, Role::User);
                assert!(
                    request
                        .messages
                        .iter()
                        .any(|m| m.role == Role::User && m.content.text() == "question")
                );
            }
            let histories = source.0.lock().unwrap();
            assert_eq!(
                histories.len(),
                requests.len(),
                "completed replay must reuse checkpoint result without new retrieval"
            );
            if mode != "react" {
                assert_ne!(histories[0], histories[1]);
            }
        }
        agent.close().await;
        state.close().await.unwrap();
    }
}

struct RecordedNode;
#[async_trait]
impl WorkflowNode for RecordedNode {
    fn kind(&self) -> &str {
        "recorded"
    }
    fn supports_resume(&self) -> bool {
        true
    }
    async fn execute(
        &self,
        input: WorkflowNodeInput,
        context: &WorkflowNodeContext,
        _: &AgentServices,
    ) -> Result<WorkflowNodeResult, AgentError> {
        let snapshot = context.conversation_snapshot().await?;
        match input.checkpoint {
            None => {
                assert_eq!(snapshot.revision, 0);
                let mut call = Message::assistant("inspect");
                call.tool_calls = vec![ToolCall::new("call", "lookup", json!({"q":"value"}))];
                let batch = vec![
                    Message::user(input.request.input),
                    call,
                    Message::tool("call", "recorded result"),
                ];
                context
                    .save_with_conversation(json!({"confirmed":true}), 0, &batch)
                    .await?;
                assert!(
                    context
                        .append_conversation(0, &[Message::assistant("stale")])
                        .await
                        .is_err()
                );
                let mut result = WorkflowNodeResult::completed(Value::Null);
                result.status = RunStatus::AwaitingInput;
                result.interactions =
                    vec![InteractionRequest::input("continue", json!({"type":"string"})).unwrap()];
                Ok(result)
            }
            Some(checkpoint) => {
                assert_eq!(checkpoint["confirmed"], true);
                assert_eq!(snapshot.revision, 3);
                assert_eq!(snapshot.messages[2].content.text(), "recorded result");
                let mut message = Message::assistant("final");
                message.structured = Some(json!({"done":true}));
                context
                    .save_with_conversation(
                        json!({"confirmed":true,"complete":true}),
                        snapshot.revision,
                        &[message],
                    )
                    .await?;
                Ok(WorkflowNodeResult::completed(json!({"done":true})))
            }
        }
    }
}
fn definition() -> DagWorkflowDefinition {
    DagWorkflowDefinition {
        id: "records".into(),
        version: 1,
        nodes: vec![DagNode::new("inspect", "recorded", json!({}))],
        edges: vec![],
        max_parallel_nodes: 1,
    }
}
async fn recorded_agent(state: Arc<SqliteStateStore>) -> Agent {
    TestBuilder::default()
        .plugin(StateStorePlugin(state))
        .plugin(WorkflowNodePlugin::new("recorded", RecordedNode))
        .plugin(AgentLoopPlugin::new(
            DagWorkflowLoop::new(definition()).unwrap(),
        ))
        .build()
        .await
        .unwrap()
}
#[tokio::test]
async fn custom_node_commits_state_and_conversation_together_and_resumes_same_history() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let store = Arc::new(
        SqliteStateStore::open(&path, SqliteOptions::default())
            .await
            .unwrap(),
    );
    let agent = recorded_agent(store.clone()).await;
    let result = agent
        .run(RunRequest::new("session", "original input"))
        .await
        .unwrap();
    assert_eq!(result.status, RunStatus::AwaitingInput);
    let checkpoint = store.latest("session").await.unwrap().unwrap();
    let state: DagWorkflowState = serde_json::from_value(checkpoint.state).unwrap();
    let original = state.nodes["inspect"]
        .conversation
        .as_ref()
        .unwrap()
        .clone();
    assert_eq!(
        state.nodes["inspect"].checkpoint.as_ref().unwrap()["confirmed"],
        true
    );
    assert_eq!(original.revision, 3);
    agent.close().await;
    store.close().await.unwrap();
    let store = Arc::new(
        SqliteStateStore::open(&path, SqliteOptions::default())
            .await
            .unwrap(),
    );
    let agent = recorded_agent(store.clone()).await;
    let mut request = RunRequest::new("session", "ignored");
    request.resume = Some(ResumeRequest {
        run_id: checkpoint.run_id,
        response: Some(InteractionResponse {
            interaction_id: result.interactions[0].id,
            value: json!("continue"),
        }),
    });
    assert_eq!(
        agent.run(request).await.unwrap().status,
        RunStatus::Completed
    );
    let state: DagWorkflowState =
        serde_json::from_value(store.load(checkpoint.run_id).await.unwrap().state).unwrap();
    let final_history = state.nodes["inspect"].conversation.as_ref().unwrap();
    assert_eq!(final_history.revision, 4);
    assert_eq!(original.message_id(2), final_history.message_id(2));
    assert_eq!(
        final_history.messages[3].structured,
        Some(json!({"done":true}))
    );
    agent.close().await;
    store.close().await.unwrap();
}
