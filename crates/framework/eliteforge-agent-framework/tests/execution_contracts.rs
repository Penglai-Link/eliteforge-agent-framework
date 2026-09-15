use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_test_support::*;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

#[derive(Clone)]
struct RecordingModel {
    requests: Arc<Mutex<Vec<ChatRequest>>>,
    response: ChatResponse,
}
#[async_trait]
impl ChatModel for RecordingModel {
    async fn generate(
        &self,
        _model_id: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        self.requests.lock().unwrap().push(request);
        Ok(self.response.clone())
    }
}
fn model(response: ChatResponse) -> RecordingModel {
    RecordingModel {
        requests: Arc::default(),
        response,
    }
}

#[tokio::test]
async fn json_and_media_results_preserve_typed_content_and_original_history() {
    for content in [
        Content(vec![ContentBlock::Json {
            value: json!({"answer": 37}),
        }]),
        Content(vec![ContentBlock::Media {
            media: MediaContent {
                mime_type: "image/png".into(),
                source: MediaSource::Base64("aW1hZ2U=".into()),
                name: Some("result.png".into()),
                estimated_tokens: None,
            },
        }]),
    ] {
        let provider = model(ChatResponse {
            content: content.clone(),
            ..Default::default()
        });
        let memory = TestConversationStore::default();
        let agent = TestBuilder::default()
            .plugin(ModelConnectionPlugin::new(ModelConnection::new(
                "default",
                Arc::new(provider),
                vec![
                    ModelDescriptor::new("default")
                        .with_output_modalities([Modality::Text, Modality::Image]),
                ],
            )))
            .plugin(ConversationStorePlugin::new(memory.clone()))
            .build()
            .await
            .unwrap();
        let mut request = RunRequest::new("typed", "produce output");
        if matches!(content.0[0], ContentBlock::Json { .. }) {
            request.generation.output_schema = Some(OutputSchema {
                name: "answer".into(),
                schema: json!({"type":"object","required":["answer"],"properties":{"answer":{"type":"integer"}},"additionalProperties":false}),
            });
        }
        let result = agent.run(request).await.unwrap();
        assert_eq!(result.content, content);
        if matches!(content.0[0], ContentBlock::Json { .. }) {
            assert_eq!(result.structured, Some(json!({"answer":37})));
        }
        assert_eq!(memory.snapshot("typed")[1].content, content);
        assert_eq!(
            serde_json::from_value::<Message>(
                serde_json::to_value(&memory.snapshot("typed")[1]).unwrap()
            )
            .unwrap()
            .content,
            content
        );
        agent.close().await;
    }
}

#[tokio::test]
async fn trimming_and_overflow_change_projection_without_deleting_history() {
    let memory = TestConversationStore::default();
    let mut session = memory.open("history").await.unwrap();
    let original = vec![
        Message::user("old"),
        Message::assistant("old answer"),
        Message::user("middle"),
        Message::assistant("middle answer"),
        Message::user("new"),
    ];
    session.append(&original).await.unwrap();
    let budget = ContextBudget {
        max_messages: 3,
        max_tokens: 1000,
    };
    let mut projection = session.snapshot().await.unwrap().messages;
    trim_context(&mut projection, budget).unwrap();
    assert_eq!(projection, original[2..]);
    trim_context(
        &mut projection,
        ContextBudget {
            max_messages: 1,
            ..budget
        },
    )
    .unwrap();
    assert_eq!(projection, original[4..]);
    assert_eq!(memory.snapshot("history"), original);
}

struct ModelLayer {
    label: &'static str,
    log: Arc<Mutex<Vec<String>>>,
}
#[async_trait]
impl ModelMiddleware for ModelLayer {
    async fn intercept(
        &self,
        mut request: ChatRequest,
        context: &ModelInvocationContext<'_>,
        next: ModelNext<'_>,
    ) -> Result<ChatResponse, ModelError> {
        self.log
            .lock()
            .unwrap()
            .push(format!("{} before", self.label));
        request
            .messages
            .insert(0, Message::system(format!("{} instruction", self.label)));
        let result = next.run(request, context).await;
        self.log
            .lock()
            .unwrap()
            .push(format!("{} after", self.label));
        result
    }
}
#[tokio::test]
async fn model_interceptors_are_ordered_and_requests_reach_real_terminal() {
    let log = Arc::default();
    let provider = model(ChatResponse::text("done"));
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(provider.clone()))
        .plugin(ModelMiddlewarePlugin::new(
            "a",
            ModelLayer {
                label: "a",
                log: Arc::clone(&log),
            },
        ))
        .plugin(ModelMiddlewarePlugin::new(
            "b",
            ModelLayer {
                label: "b",
                log: Arc::clone(&log),
            },
        ))
        .build()
        .await
        .unwrap();
    agent.run(RunRequest::new("order", "input")).await.unwrap();
    assert_eq!(
        *log.lock().unwrap(),
        ["a before", "b before", "b after", "a after"]
    );
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].messages[0].content.text(), "b instruction");
    assert_eq!(requests[0].messages[1].content.text(), "a instruction");
}

struct Redirect;
#[async_trait]
impl ModelMiddleware for Redirect {
    async fn intercept(
        &self,
        request: ChatRequest,
        context: &ModelInvocationContext<'_>,
        next: ModelNext<'_>,
    ) -> Result<ChatResponse, ModelError> {
        let model = ModelRef::new("default", "outside-catalog");
        let forged = ModelInvocationContext {
            model: &model,
            ..*context
        };
        next.run(request, &forged).await
    }
}
#[tokio::test]
async fn middleware_cannot_redirect_to_uncatalogued_model() {
    let provider = model(ChatResponse::text("must not run"));
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(provider.clone()))
        .plugin(ModelMiddlewarePlugin::new("redirect", Redirect))
        .build()
        .await
        .unwrap();
    assert!(
        agent
            .run(RunRequest::new("redirect", "input"))
            .await
            .is_err()
    );
    assert!(provider.requests.lock().unwrap().is_empty());
}

struct Retrieval;
#[async_trait]
impl ContextContributor for Retrieval {
    fn block_id(&self) -> &str {
        "retrieved"
    }
    async fn contribute(&self, _: &ContextInput<'_>) -> Result<ContextBlock, AgentError> {
        Ok(ContextBlock::data("retrieved", "data".repeat(100)))
    }
}
#[tokio::test]
async fn optional_context_never_displaces_required_input() {
    let provider = model(ChatResponse::text("ok"));
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(provider.clone()))
        .plugin(ContextContributorPlugin::new("retrieval", Retrieval))
        .plugin(ContextLayoutPlugin(ContextLayout(vec![
            ContextBlockConfig::new(INSTRUCTIONS_BLOCK, BlockBudget::tokens(0)),
            ContextBlockConfig::new("retrieved", BlockBudget::tokens(20)).optional(),
            ContextBlockConfig::new(HISTORY_BLOCK, BlockBudget::tokens(80)),
        ])))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("context", "current question");
    request.limits.max_context_tokens = 100;
    agent.run(request).await.unwrap();
    assert_eq!(
        provider.requests.lock().unwrap()[0].messages,
        vec![Message::user("current question")]
    );
}

struct SideEffect(Arc<AtomicUsize>);
#[async_trait]
impl Tool for SideEffect {
    async fn call(&self, _: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::text("executed"))
    }
}
#[tokio::test]
async fn schema_failure_prevents_tool_side_effect_and_records_error_flag() {
    let effects = Arc::new(AtomicUsize::new(0));
    let memory = TestConversationStore::default();
    let provider = model(ChatResponse::tool_calls(vec![ToolCall::new(
        "call",
        "write",
        json!({"amount":"wrong type"}),
    )]));
    let agent = TestBuilder::default().plugin(ChatModelPlugin::new(provider)).plugin(ConversationStorePlugin::new(memory.clone()))
        .plugin(ToolProviderPlugin::new("tools", StaticToolProvider::new(vec![ToolBinding::new(ToolDescriptor::new("write", "write", "effect", json!({"type":"object","properties":{"amount":{"type":"integer"}},"required":["amount"]})), SideEffect(effects.clone()))]))).build().await.unwrap();
    let mut request = RunRequest::new("schema", "write");
    request.limits.max_steps = 1;
    agent.run(request).await.unwrap();
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    assert!(
        memory
            .snapshot("schema")
            .last()
            .unwrap()
            .tool_result
            .as_ref()
            .unwrap()
            .is_error
    );
}

#[tokio::test]
async fn insufficient_total_budget_prevents_first_provider_call() {
    let provider = model(ChatResponse::text("expensive"));
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(provider.clone()))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("budget", "question");
    request.limits.max_total_tokens = 1;
    assert!(agent.run(request).await.is_err());
    assert!(provider.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn reported_budget_overrun_prevents_subsequent_tool_effects() {
    let effects = Arc::new(AtomicUsize::new(0));
    let provider = model(ChatResponse {
        tool_calls: vec![ToolCall::new("effect", "write", json!({}))],
        usage: Usage {
            input_tokens: 1000,
            output_tokens: 5000,
        },
        ..Default::default()
    });
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(provider.clone()))
        .plugin(ToolProviderPlugin::new(
            "tools",
            StaticToolProvider::new(vec![ToolBinding::new(
                ToolDescriptor::new("write", "write", "effect", json!({})),
                SideEffect(effects.clone()),
            )]),
        ))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("overrun", "question");
    request.limits.max_total_tokens = 500;
    assert!(agent.run(request).await.is_err());
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    assert!(
        provider.requests.lock().unwrap()[0]
            .options
            .max_output_tokens
            .unwrap()
            < 500
    );
}

struct Never;
#[async_trait]
impl Tool for Never {
    async fn call(&self, _: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        std::future::pending().await
    }
}
struct ExitCount(Arc<AtomicUsize>);
#[async_trait]
impl ToolMiddleware for ExitCount {
    async fn intercept(
        &self,
        _: &ToolInvocationContext<'_>,
        next: ToolNext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let result = next.run().await;
        assert!(result.is_err());
        self.0.fetch_add(1, Ordering::SeqCst);
        result
    }
}

#[tokio::test]
async fn tool_timeout_unwinds_entered_middleware() {
    let exited = Arc::new(AtomicUsize::new(0));
    let provider = model(ChatResponse::tool_calls(vec![ToolCall::new(
        "call",
        "wait",
        json!({}),
    )]));
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(provider))
        .plugin(ToolMiddlewarePlugin::new("exit", ExitCount(exited.clone())))
        .plugin(ToolProviderPlugin::new(
            "tools",
            StaticToolProvider::new(vec![ToolBinding::new(
                ToolDescriptor::new("wait", "wait", "wait", json!({})),
                Never,
            )]),
        ))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("timeout", "wait");
    request.limits.max_steps = 1;
    request.limits.tool_timeout = Duration::from_millis(10);
    let error = agent.run(request).await.unwrap_err();
    assert!(matches!(error, AgentError::Run(_)));
    assert!(error.to_string().contains("tool timed out"));
    assert_eq!(exited.load(Ordering::SeqCst), 1);
}

struct Flood {
    emitted: Arc<AtomicUsize>,
}
#[async_trait]
impl ChatModel for Flood {
    async fn generate(
        &self,
        _model_id: &str,
        _: ChatRequest,
        stream: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        for _ in 0..1000 {
            stream.text_delta("x").await;
            self.emitted.fetch_add(1, Ordering::SeqCst);
        }
        Ok(ChatResponse::text("done"))
    }
}
struct SlowObserver {
    gate: Arc<tokio::sync::Semaphore>,
    finished: Arc<AtomicUsize>,
}
#[async_trait]
impl EventSink for SlowObserver {
    async fn on_event(&self, _: &RunContext, event: &AgentEvent) {
        if matches!(event, AgentEvent::RunStarted { .. }) {
            self.gate.acquire().await.unwrap().forget();
        }
        if matches!(event, AgentEvent::RunFinished { .. }) {
            self.finished.fetch_add(1, Ordering::SeqCst);
        }
    }
}
#[tokio::test]
async fn slow_observer_does_not_block_stream_or_drop_terminal_on_saturation() {
    let emitted = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(Flood {
            emitted: emitted.clone(),
        }))
        .plugin(EventPlugin::new(
            "slow",
            SlowObserver {
                gate: gate.clone(),
                finished: finished.clone(),
            },
        ))
        .build()
        .await
        .unwrap();
    let handle = agent.start(RunRequest::new("stream", "input")).unwrap();
    tokio::time::timeout(Duration::from_millis(500), async {
        while emitted.load(Ordering::SeqCst) != 1000 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    gate.add_permits(1);
    agent.wait(handle).await.unwrap();
    assert_eq!(finished.load(Ordering::SeqCst), 1);
}

struct TerminalObserver {
    seen: Arc<AtomicUsize>,
    stall_terminal: bool,
}
#[async_trait]
impl EventSink for TerminalObserver {
    async fn on_event(&self, _: &RunContext, event: &AgentEvent) {
        if matches!(
            event,
            AgentEvent::RunFinished { .. } | AgentEvent::RunFailed(_) | AgentEvent::RunCancelled
        ) {
            self.seen.fetch_add(1, Ordering::SeqCst);
            if !self.stall_terminal {
                return;
            }
        }
        std::future::pending().await
    }
}
struct TerminalLoop(u8);
#[async_trait]
impl AgentLoop for TerminalLoop {
    async fn run(
        &self,
        _: &RunRequest,
        context: &RunContext,
        _: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        for _ in 0..300 {
            context.emit(AgentEvent::TextDelta("x".into())).await;
        }
        match self.0 {
            1 => Err(AgentError::Run("expected failure".into())),
            2 => Err(AgentError::Cancelled),
            _ => Ok(RunResult {
                output: "done".into(),
                content: "done".into(),
                structured: None,
                interactions: vec![],
                status: RunStatus::Completed,
                usage: Usage::default(),
                steps: 0,
                tool_calls: 0,
            }),
        }
    }
}
#[tokio::test]
async fn terminal_callbacks_have_a_budget_independent_of_backlog_and_other_sinks() {
    for outcome in 0..3 {
        let slow = Arc::new(AtomicUsize::new(0));
        let fast = Arc::new(AtomicUsize::new(0));
        let agent = TestBuilder::default()
            .plugin(AgentLoopPlugin::new(TerminalLoop(outcome)))
            .plugin(EventPlugin::new(
                "a-stalled",
                TerminalObserver {
                    seen: slow.clone(),
                    stall_terminal: true,
                },
            ))
            .plugin(EventPlugin::new(
                "b-responsive",
                TerminalObserver {
                    seen: fast.clone(),
                    stall_terminal: false,
                },
            ))
            .build()
            .await
            .unwrap();
        let mut request = RunRequest::new("terminal", "input");
        request.limits.cleanup_timeout = Duration::from_millis(30);
        let answer = tokio::time::timeout(Duration::from_secs(2), agent.run(request))
            .await
            .unwrap();
        match outcome {
            0 => assert!(answer.is_ok()),
            1 => assert!(matches!(answer, Err(AgentError::Run(_)))),
            _ => assert!(matches!(answer, Err(AgentError::Cancelled))),
        }
        assert_eq!(
            slow.load(Ordering::SeqCst),
            1,
            "stalled sink still receives a terminal attempt"
        );
        assert_eq!(
            fast.load(Ordering::SeqCst),
            1,
            "responsive sink receives the terminal exactly once"
        );
        agent.close().await;
    }
}

#[test]
fn structured_results_are_accounted_and_external_schema_references_fail_closed() {
    let mut message = Message::tool("call", "short");
    let initial = estimate_message_tokens(&message);
    message.tool_result = Some(ToolResultMetadata {
        is_error: false,
        structured: Some(json!({"large":"x".repeat(10_000)})),
        result_reference: None,
    });
    assert!(estimate_message_tokens(&message) > initial + 10_000);
    assert!(compile_schema(&json!({"$ref":"file:///must-not-be-read.json"})).is_err());
    assert!(
        compile_schema(&json!({"$ref":"https://example.invalid/must-not-be-fetched"})).is_err()
    );
}
