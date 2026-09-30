use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_test_support::*;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Provider {
    calls: Mutex<Vec<(String, Vec<Message>)>>,
    fail: bool,
    emit_before_failure: bool,
}
#[async_trait]
impl ChatModel for Provider {
    async fn generate(
        &self,
        model: &str,
        request: ChatRequest,
        stream: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        self.calls
            .lock()
            .unwrap()
            .push((model.into(), request.messages));
        if self.fail {
            if self.emit_before_failure {
                stream.text_delta("partial").await;
            }
            return Err(ModelError::Transient("busy".into()));
        }
        Ok(ChatResponse::text(model))
    }
}
fn connection(id: &str, provider: Arc<Provider>, models: &[&str]) -> ModelConnectionPlugin {
    ModelConnectionPlugin::new(ModelConnection::new(
        id,
        provider,
        models.iter().map(|id| ModelDescriptor::new(*id)).collect(),
    ))
}

#[tokio::test]
async fn one_connection_multiple_models_and_composed_stage_routing() {
    let provider = Arc::new(Provider::default());
    let strategy = StageModelStrategy {
        stages: [
            (
                "plan".into(),
                Arc::new(FixedModelStrategy(ModelRef::new("plan", "planner")))
                    as Arc<dyn ModelSelectionStrategy>,
            ),
            (
                "execute".into(),
                Arc::new(RoutingStrategy {
                    candidates: vec![ModelRef::new("plan", "executor")],
                }),
            ),
        ]
        .into(),
        fallback: None,
    };
    let agent = TestBuilder::default()
        .plugin(connection(
            "plan",
            provider.clone(),
            &["planner", "executor"],
        ))
        .plugin(ModelStrategyPlugin(Arc::new(strategy)))
        .build()
        .await
        .unwrap();
    for stage in ["plan", "execute"] {
        let mut request = RunRequest::new("shared", "next phase");
        request.model_stage = stage.into();
        agent.run(request).await.unwrap();
    }
    let calls = provider.calls.lock().unwrap().clone();
    assert_eq!(
        calls.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(),
        ["planner", "executor"]
    );
    assert!(calls[1].1.iter().any(|m| m.content.text() == "planner"));
    drop(calls);
    agent.close().await;
}

#[tokio::test]
async fn invalid_catalog_fails_at_build_and_ambiguity_fails_at_dispatch() {
    let provider = Arc::new(Provider::default());
    let agent = TestBuilder::default()
        .plugin(connection("x", provider.clone(), &["a", "b"]))
        .build()
        .await
        .unwrap();
    assert!(agent.run(RunRequest::new("ambiguous", "hi")).await.is_err());
    let mut explicit = RunRequest::new("explicit", "hi");
    explicit.model = Some(ModelRef::new("x", "b"));
    assert_eq!(agent.run(explicit).await.unwrap().output, "b");
    agent.close().await;
    provider.calls.lock().unwrap().clear();
    assert!(
        TestBuilder::default()
            .plugin(connection("x", provider.clone(), &["a", "a"]))
            .build()
            .await
            .is_err()
    );
    assert!(
        TestBuilder::default()
            .plugin(connection("x", provider.clone(), &["a"]))
            .plugin(ModelStrategyPlugin(Arc::new(FixedModelStrategy(
                ModelRef::new("x", "hidden")
            ))))
            .build()
            .await
            .is_err()
    );
    assert!(provider.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn routing_rebuilds_context_for_smaller_model_and_reports_actual_target() {
    let failed = Arc::new(Provider {
        fail: true,
        ..Default::default()
    });
    let healthy = Arc::new(Provider::default());
    let memory = TestConversationStore::default();
    let mut session = memory.open("s").await.unwrap();
    session
        .append(&[
            Message::user("old ".repeat(100)),
            Message::assistant("old answer"),
        ])
        .await
        .unwrap();
    drop(session);
    let mut small = ModelDescriptor::new("small");
    small.max_input_tokens = Some(100);
    let agent = TestBuilder::default()
        .plugin(ConversationStorePlugin::new(memory))
        .plugin(connection("primary", failed.clone(), &["large"]))
        .plugin(ModelConnectionPlugin::new(ModelConnection::new(
            "backup",
            healthy.clone(),
            vec![small],
        )))
        .plugin(ModelStrategyPlugin(Arc::new(RoutingStrategy {
            candidates: vec![
                ModelRef::new("primary", "large"),
                ModelRef::new("backup", "small"),
            ],
        })))
        .build()
        .await
        .unwrap();
    let mut handle = agent.start(RunRequest::new("s", "new")).unwrap();
    let mut events = agent.events(&mut handle).unwrap();
    assert_eq!(agent.wait(handle).await.unwrap().output, "small");
    let primary = failed.calls.lock().unwrap().clone();
    let backup = healthy.calls.lock().unwrap().clone();
    assert_eq!(primary[0].1.len(), 3);
    assert_eq!(backup[0].1.len(), 1);
    let mut selected = Vec::new();
    let mut usage = Vec::new();
    while let Ok(event) = events.try_recv() {
        match event.event {
            AgentEvent::ModelSelected { model, .. } => selected.push(model),
            AgentEvent::ModelUsage { model, .. } => usage.push(model),
            _ => {}
        }
    }
    assert_eq!(
        selected,
        [
            ModelRef::new("primary", "large"),
            ModelRef::new("backup", "small")
        ]
    );
    assert_eq!(usage, [ModelRef::new("backup", "small")]);
    drop(primary);
    drop(backup);
    agent.close().await;
}

#[tokio::test]
async fn streaming_output_prevents_failover_and_retry_budget_is_shared() {
    for partial in [false, true] {
        let failed = Arc::new(Provider {
            fail: true,
            emit_before_failure: partial,
            ..Default::default()
        });
        let backup = Arc::new(Provider {
            fail: true,
            ..Default::default()
        });
        let agent = TestBuilder::default()
            .plugin(connection("a", failed.clone(), &["a"]))
            .plugin(connection("b", backup.clone(), &["b"]))
            .plugin(ModelStrategyPlugin(Arc::new(RoutingStrategy {
                candidates: vec![ModelRef::new("a", "a"), ModelRef::new("b", "b")],
            })))
            .build()
            .await
            .unwrap();
        let mut request = RunRequest::new("s", "test");
        request.limits.max_model_retries = 1;
        assert!(agent.run(request).await.is_err());
        assert_eq!(failed.calls.lock().unwrap().len(), 1);
        assert_eq!(backup.calls.lock().unwrap().len(), usize::from(!partial));
        agent.close().await;
    }
}

struct PhaseLoop;
#[async_trait]
impl AgentLoop for PhaseLoop {
    fn requires_model(&self) -> bool {
        true
    }
    async fn run(
        &self,
        request: &RunRequest,
        context: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let mut memory = services.conversations.open(&request.session_id).await?;
        memory.append(&[Message::user(&request.input)]).await?;
        let mut output = String::new();
        for (index, stage) in ["plan", "execute"].iter().enumerate() {
            let response = services
                .models
                .as_ref()
                .unwrap()
                .invoke(
                    ModelCall {
                        stage,
                        step: index + 1,
                        instructions: &[],
                        tools: &[],
                        limits: &request.limits,
                        options: &request.generation,
                    },
                    memory.as_mut(),
                    context,
                )
                .await?;
            memory
                .append(&[Message::assistant(&response.content)])
                .await?;
            output = response.content.text();
        }
        memory.finish(ConversationOutcome::Completed).await?;
        Ok(RunResult {
            interactions: Vec::new(),
            content: Content::default(),
            structured: None,
            output,
            status: RunStatus::Completed,
            usage: Usage::default(),
            steps: 2,
            tool_calls: 0,
            external_session: None,
        })
    }
}

#[tokio::test]
async fn custom_loop_changes_models_within_one_run() {
    let provider = Arc::new(Provider::default());
    let strategy = StageModelStrategy {
        stages: ["plan", "execute"]
            .into_iter()
            .map(|stage| {
                (
                    stage.into(),
                    Arc::new(FixedModelStrategy(ModelRef::new("c", stage)))
                        as Arc<dyn ModelSelectionStrategy>,
                )
            })
            .collect(),
        fallback: None,
    };
    let agent = TestBuilder::default()
        .plugin(connection("c", provider.clone(), &["plan", "execute"]))
        .plugin(ModelStrategyPlugin(Arc::new(strategy)))
        .plugin(AgentLoopPlugin::new(PhaseLoop))
        .build()
        .await
        .unwrap();
    assert_eq!(
        agent
            .run(RunRequest::new("s", "work"))
            .await
            .unwrap()
            .output,
        "execute"
    );
    assert_eq!(provider.calls.lock().unwrap().len(), 2);
    agent.close().await;
}
