use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_test_support::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
#[tokio::test]
async fn memory_keeps_complete_turns_and_instructions() {
    let memory = TestConversationStore::default();
    let mut session = memory.open("s").await.unwrap();
    let call = ToolCall::new("c", "lookup", serde_json::json!({}));
    let mut assistant = Message::assistant("");
    assistant.tool_calls.push(call);
    session
        .append(&[
            Message::system("fixed"),
            Message::user("old"),
            assistant,
            Message::tool("c", "result"),
            Message::assistant("done"),
            Message::user("new"),
        ])
        .await
        .unwrap();
    let mut prepared = session.snapshot().await.unwrap().messages;
    trim_context(
        &mut prepared,
        ContextBudget {
            max_messages: 2,
            max_tokens: 1000,
        },
    )
    .unwrap();
    assert_eq!(
        prepared,
        vec![Message::system("fixed"), Message::user("new")]
    );
}

#[tokio::test]
async fn memory_rejects_orphan_results_atomically() {
    let memory = TestConversationStore::default();
    let mut session = memory.open("s").await.unwrap();
    session.append(&[Message::user("question")]).await.unwrap();
    assert!(
        session
            .append(&[Message::tool("unknown", "bad")])
            .await
            .is_err()
    );
    assert_eq!(memory.snapshot("s"), vec![Message::user("question")]);
}

#[tokio::test]
async fn memory_does_not_evict_active_sessions() {
    let memory = TestConversationStore::new(TestConversationOptions {
        max_sessions: 1,
        ..TestConversationOptions::default()
    })
    .unwrap();
    let session = memory.open("first").await.unwrap();
    assert!(memory.open("second").await.is_err());
    drop(session);
    assert!(memory.open("second").await.is_ok());
}

#[tokio::test]
async fn memory_budget_rejects_oversized_latest_turn() {
    let memory = TestConversationStore::default();
    let mut session = memory.open("s").await.unwrap();
    session
        .append(&[Message::user("long current turn")])
        .await
        .unwrap();
    assert!(matches!(
        trim_context(
            &mut session.snapshot().await.unwrap().messages,
            ContextBudget {
                max_messages: 1,
                max_tokens: 1
            }
        ),
        Err(ContextError::BudgetExceeded(_))
    ));
    assert_eq!(memory.snapshot("s").len(), 1);
}

#[tokio::test]
async fn memory_rejects_tool_calls_hidden_in_tool_result() {
    let memory = TestConversationStore::default();
    let mut session = memory.open("s").await.unwrap();
    let mut assistant = Message::assistant("");
    assistant
        .tool_calls
        .push(ToolCall::new("c", "lookup", serde_json::json!({})));
    let mut invalid_result = Message::tool("c", "result");
    invalid_result
        .tool_calls
        .push(ToolCall::new("injected", "other", serde_json::json!({})));
    assert!(
        session
            .append(&[Message::user("q"), assistant, invalid_result])
            .await
            .is_err()
    );
    assert!(memory.snapshot("s").is_empty());
}

#[tokio::test]
async fn memory_expiry_never_discards_an_active_lease() {
    let memory = TestConversationStore::new(TestConversationOptions {
        session_ttl: std::time::Duration::from_millis(5),
        ..TestConversationOptions::default()
    })
    .unwrap();
    let mut session = memory.open("s").await.unwrap();
    session.append(&[Message::user("retained")]).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    assert!(
        memory.open("s").await.is_err(),
        "an active expired session must remain busy"
    );
    assert_eq!(memory.snapshot("s").len(), 1);
    drop(session);
    tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    let mut reopened = memory.open("s").await.unwrap();
    assert!(reopened.snapshot().await.unwrap().messages.is_empty());
}

#[test]
fn tool_lease_cleans_all_callbacks_in_reverse_order_even_after_panic() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let a = seen.clone();
    let b = seen.clone();
    let c = seen.clone();
    let lease = ToolLease::new(Vec::new())
        .with_cleanup(move || a.lock().unwrap().push("first"))
        .with_cleanup(move || {
            b.lock().unwrap().push("second");
            panic!("cleanup failure");
        })
        .with_cleanup(move || c.lock().unwrap().push("third"));
    drop(lease);
    assert_eq!(*seen.lock().unwrap(), vec!["third", "second", "first"]);
}

#[derive(Clone)]
struct StreamingFailure {
    attempts: Arc<AtomicUsize>,
    reasoning: bool,
}

#[async_trait]
impl ChatModel for StreamingFailure {
    async fn generate(
        &self,
        _model_id: &str,
        _: ChatRequest,
        stream: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        if self.reasoning {
            stream.reasoning_delta("visible reasoning").await;
        } else {
            stream.text_delta("visible prefix").await;
        }
        Err(ModelError::Transient(
            "connection ended after streaming".into(),
        ))
    }
}

#[tokio::test]
async fn retry_does_not_duplicate_text_or_reasoning_already_streamed() {
    for reasoning in [false, true] {
        let attempts = Arc::new(AtomicUsize::new(0));
        let agent = TestBuilder::default()
            .plugin(ChatModelPlugin::new(StreamingFailure {
                attempts: attempts.clone(),
                reasoning,
            }))
            .build()
            .await
            .unwrap();
        let mut handle = agent.start(RunRequest::new("s", "hello")).unwrap();
        let mut events = handle.take_events();
        assert!(matches!(
            handle.wait().await,
            Err(AgentError::Model(ModelError::Transient(_)))
        ));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        let mut terminal_count = 0;
        let mut deltas = 0;
        while let Ok(envelope) = events.try_recv() {
            match envelope.event {
                AgentEvent::TextDelta(_) | AgentEvent::ReasoningDelta(_) => deltas += 1,
                AgentEvent::RunFailed(_) => terminal_count += 1,
                AgentEvent::ModelRetry { .. } => panic!("retry would duplicate visible deltas"),
                _ => {}
            }
        }
        assert_eq!((deltas, terminal_count), (1, 1));
    }
}

#[derive(Clone)]
struct RecoverableMemory {
    oversized: bool,
}

struct RecoverableSession {
    messages: Vec<Message>,
}

#[async_trait]
impl ConversationStore for RecoverableMemory {
    async fn open(&self, _: &str) -> Result<Box<dyn ConversationSession>, ConversationError> {
        Ok(Box::new(RecoverableSession {
            messages: if self.oversized {
                vec![Message::system("x".repeat(2000))]
            } else {
                vec![Message::user("old turn"), Message::assistant("old answer")]
            },
        }))
    }
}

#[async_trait]
impl ConversationSession for RecoverableSession {
    async fn snapshot(&mut self) -> Result<ConversationSnapshot, ConversationError> {
        Ok(ConversationSnapshot::new(
            ConversationKey::new("recoverable"),
            "fixture",
            self.messages.clone(),
        ))
    }

    async fn append(&mut self, messages: &[Message]) -> Result<(), ConversationError> {
        self.messages.extend_from_slice(messages);
        Ok(())
    }
}

struct RecoveryCounter(Arc<AtomicUsize>);
#[async_trait]
impl EventSink for RecoveryCounter {
    async fn on_event(&self, _: &RunContext, event: &AgentEvent) {
        if matches!(event, AgentEvent::ContextRecovered) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
}

#[derive(Clone)]
struct OverflowModel {
    attempts: Arc<AtomicUsize>,
    always_overflow: bool,
}

#[async_trait]
impl ChatModel for OverflowModel {
    async fn generate(
        &self,
        _model_id: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
        if self.always_overflow || attempt == 0 {
            return Err(ModelError::ContextOverflow("too large".into()));
        }
        assert_eq!(request.messages, vec![Message::user("new input")]);
        Ok(ChatResponse::text("recovered"))
    }
}

#[tokio::test]
async fn typed_overflow_reduces_context_and_rebuilds_request() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let recovered = Arc::new(AtomicUsize::new(0));
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(OverflowModel {
            attempts: attempts.clone(),
            always_overflow: false,
        }))
        .plugin(ConversationStorePlugin::new(RecoverableMemory {
            oversized: false,
        }))
        .plugin(EventPlugin::new(
            "recovery-events",
            RecoveryCounter(recovered.clone()),
        ))
        .build()
        .await
        .unwrap();
    assert_eq!(
        agent
            .run(RunRequest::new("s", "new input"))
            .await
            .unwrap()
            .output,
        "recovered"
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(recovered.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn overflow_recovery_is_bounded() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let recovered = Arc::new(AtomicUsize::new(0));
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(OverflowModel {
            attempts: attempts.clone(),
            always_overflow: true,
        }))
        .plugin(ConversationStorePlugin::new(RecoverableMemory {
            oversized: false,
        }))
        .plugin(EventPlugin::new(
            "recovery-events",
            RecoveryCounter(recovered.clone()),
        ))
        .build()
        .await
        .unwrap();
    assert!(matches!(
        agent.run(RunRequest::new("s", "new input")).await,
        Err(AgentError::Model(ModelError::ContextOverflow(_)))
    ));
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(recovered.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn framework_checks_fixed_instructions_against_context_budget() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(OverflowModel {
            attempts: attempts.clone(),
            always_overflow: false,
        }))
        .plugin(ConversationStorePlugin::new(RecoverableMemory {
            oversized: true,
        }))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("s", "new input");
    request.limits.max_context_tokens = 100;
    assert!(matches!(
        agent.run(request).await.unwrap_err().primary(),
        AgentError::Context(ContextError::BudgetExceeded(_))
    ));
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
}

struct NeverCalledTool;

#[async_trait]
impl Tool for NeverCalledTool {
    async fn call(&self, _: serde_json::Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        panic!("tool must not be called")
    }
}

#[tokio::test]
async fn exposed_tool_schemas_count_against_context_budget() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let descriptor = ToolDescriptor::new(
        "stable.tool",
        "tool",
        "long description".repeat(100),
        serde_json::json!({"type": "object"}),
    );
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(OverflowModel {
            attempts: attempts.clone(),
            always_overflow: false,
        }))
        .plugin(ToolProviderPlugin::new(
            "tools",
            StaticToolProvider::new(vec![ToolBinding::new(descriptor, NeverCalledTool)]),
        ))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("s", "new input");
    request.limits.max_context_tokens = 100;
    assert!(matches!(
        agent.run(request).await.unwrap_err().primary(),
        AgentError::Context(ContextError::BudgetExceeded(_))
    ));
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
}
