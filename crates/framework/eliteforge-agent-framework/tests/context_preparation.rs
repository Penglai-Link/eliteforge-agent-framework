use eliteforge_agent_framework::*;
use eliteforge_agent_test_support::TestBuilder;
use std::sync::{Arc, Mutex};
#[derive(Clone, Default)]
struct Model(Arc<Mutex<Vec<String>>>);
#[async_trait::async_trait]
impl ChatModel for Model {
    async fn generate(
        &self,
        model: &str,
        _: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        self.0.lock().unwrap().push(model.into());
        if model == "failure" {
            return Err(ModelError::Fatal("summary failed".into()));
        }
        Ok(ChatResponse::text(if model == "summary" {
            "compressed"
        } else {
            "answer"
        }))
    }
}
struct Completion(Arc<Mutex<Vec<String>>>);
#[async_trait::async_trait]
impl ContextReductionCompletion for Completion {
    async fn complete(&self, result: Result<ChatResponse, AgentError>) -> Result<(), AgentError> {
        self.0.lock().unwrap().push(
            result
                .map(|r| r.content.text())
                .unwrap_or_else(|_| "failed".into()),
        );
        Ok(())
    }
}
struct Policy {
    completed: Arc<Mutex<Vec<String>>>,
    repeat: bool,
    fail: bool,
}
#[async_trait::async_trait]
impl ContextPreparationPolicy for Policy {
    async fn prepare(
        &self,
        input: &ContextAssemblyInput<'_>,
        reductions: usize,
    ) -> Result<ContextPreparation, AgentError> {
        if reductions > 0 && !self.repeat {
            return DefaultContextAssembler::default()
                .assemble(input)
                .await
                .map(ContextPreparation::Ready);
        }
        Ok(ContextPreparation::Reduce(ContextReductionJob {
            workflow: None,
            source_revision: "revision-1".into(),
            model: ModelRef::new("model", if self.fail { "failure" } else { "summary" }),
            request: ChatRequest {
                options: Default::default(),
                messages: vec![Message::user("summarize")],
                tools: vec![],
                extensions: input.run.extensions.clone(),
            },
            stage: "summary".into(),
            limits: input.limits.clone(),
            completion: Arc::new(Completion(self.completed.clone())),
        }))
    }
}
async fn setup(repeat: bool, fail: bool) -> (Agent, Model, Arc<Mutex<Vec<String>>>) {
    let model = Model::default();
    let completed = Arc::new(Mutex::new(vec![]));
    let agent = TestBuilder::default()
        .default_models(false)
        .plugin(ModelConnectionPlugin::new(ModelConnection::new(
            "model",
            Arc::new(model.clone()),
            vec![
                ModelDescriptor::new("main"),
                ModelDescriptor::new("summary"),
                ModelDescriptor::new("failure"),
            ],
        )))
        .plugin(ContextAssemblerPlugin::new(PreparingContextAssembler::new(
            Policy {
                completed: completed.clone(),
                repeat,
                fail,
            },
            2,
        )))
        .build()
        .await
        .unwrap();
    (agent, model, completed)
}
fn request() -> RunRequest {
    let mut request = RunRequest::new("session", "question");
    request.model = Some(ModelRef::new("model", "main"));
    request
}
#[tokio::test]
async fn auxiliary_reduction_finishes_before_answer_model() {
    let (agent, model, completed) = setup(false, false).await;
    agent.run(request()).await.unwrap();
    assert_eq!(*model.0.lock().unwrap(), ["summary", "main"]);
    assert_eq!(*completed.lock().unwrap(), ["compressed"]);
    agent.framework().close().await;
}
#[tokio::test]
async fn unchanged_source_is_rejected_before_a_second_summary_call() {
    let (agent, model, _) = setup(true, false).await;
    let error = agent.run(request()).await.unwrap_err();
    assert!(error.to_string().contains("no source progress"), "{error}");
    assert_eq!(*model.0.lock().unwrap(), ["summary"]);
    agent.framework().close().await;
}
#[tokio::test]
async fn failed_summary_cannot_be_silently_accepted_by_completion_hook() {
    let (agent, model, completed) = setup(false, true).await;
    assert!(agent.run(request()).await.is_err());
    assert_eq!(*model.0.lock().unwrap(), ["failure"]);
    assert_eq!(*completed.lock().unwrap(), ["failed"]);
    agent.framework().close().await;
}

#[tokio::test]
async fn auxiliary_calls_reserve_the_last_dispatch_for_the_answer() {
    let (agent, model, completed) = setup(false, false).await;
    let mut request = request();
    request.limits.max_model_calls_per_step = Some(1);
    assert!(agent.run(request).await.is_err());
    assert!(model.0.lock().unwrap().is_empty());
    assert_eq!(*completed.lock().unwrap(), ["failed"]);
    agent.framework().close().await;
}

struct AccountBoundModel {
    revision: usize,
    current_revision: Arc<std::sync::atomic::AtomicUsize>,
    calls: Arc<Mutex<Vec<String>>>,
}
#[async_trait::async_trait]
impl ChatModel for AccountBoundModel {
    async fn generate(
        &self,
        model: &str,
        _: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        if self
            .current_revision
            .load(std::sync::atomic::Ordering::SeqCst)
            != self.revision
        {
            return Err(ModelError::Provider(Box::new(ProviderModelError {
                authorization_attempt: None,
                provider: "test".into(),
                category: ProviderErrorCategory::AccountChanged,
                http_status: None,
                business_code: None,
                error_type: None,
                model: None,
                message: "account changed before dispatch".into(),
            })));
        }
        self.calls.lock().unwrap().push(model.into());
        Ok(ChatResponse::text("bound answer"))
    }
}

struct RefreshDuringPreparation {
    runtime: Arc<ModelRuntime>,
    replacement: ModelRuntimeUpdate,
    current_revision: Arc<std::sync::atomic::AtomicUsize>,
    completed: Arc<Mutex<Vec<String>>>,
}
#[async_trait::async_trait]
impl ContextPreparationPolicy for RefreshDuringPreparation {
    async fn prepare(
        &self,
        input: &ContextAssemblyInput<'_>,
        reductions: usize,
    ) -> Result<ContextPreparation, AgentError> {
        if reductions > 0 {
            return DefaultContextAssembler::default()
                .assemble(input)
                .await
                .map(ContextPreparation::Ready);
        }
        // Deterministically refresh between capture of the step snapshot and
        // dispatch of its reduction, as a host account notification would.
        self.current_revision.store(
            self.replacement.revision as usize,
            std::sync::atomic::Ordering::SeqCst,
        );
        self.runtime
            .replace_connection("personai", self.replacement.clone())?;
        Ok(ContextPreparation::Reduce(ContextReductionJob {
            workflow: None,
            source_revision: "alice-history".into(),
            model: ModelRef::new("personai", "summary"),
            request: ChatRequest {
                messages: vec![Message::user("private transcript for alice")],
                options: Default::default(),
                tools: vec![],
                extensions: input.run.extensions.clone(),
            },
            stage: "summary".into(),
            limits: input.limits.clone(),
            completion: Arc::new(Completion(self.completed.clone())),
        }))
    }
}

#[tokio::test]
async fn auxiliary_reduction_keeps_step_bindings_and_never_uses_a_new_account() {
    for switch_account in [false, true] {
        let revision = Arc::new(std::sync::atomic::AtomicUsize::new(1));
        let old_calls = Arc::new(Mutex::new(vec![]));
        let new_calls = Arc::new(Mutex::new(vec![]));
        let completed = Arc::new(Mutex::new(vec![]));
        let bindings = |revision_id, calls| {
            ModelConnection::new(
                "personai",
                Arc::new(AccountBoundModel {
                    revision: revision_id,
                    current_revision: revision.clone(),
                    calls,
                }),
                vec![
                    ModelDescriptor::new("main"),
                    ModelDescriptor::new("summary"),
                ],
            )
        };
        let runtime = ModelRuntime::new(ModelRuntimeUpdate::new("", 0, vec![])).unwrap();
        runtime
            .replace_connection(
                "personai",
                ModelRuntimeUpdate::new("alice", 1, vec![bindings(1, old_calls.clone())]),
            )
            .unwrap();
        let next_revision = if switch_account { 2 } else { 1 };
        let replacement = ModelRuntimeUpdate::new(
            if switch_account { "bob" } else { "alice" },
            next_revision as u64,
            vec![bindings(next_revision, new_calls.clone())],
        );
        let agent = TestBuilder::default()
            .default_models(false)
            .plugin(ModelRuntimePlugin::new(runtime.clone()))
            .plugin(ContextAssemblerPlugin::new(PreparingContextAssembler::new(
                RefreshDuringPreparation {
                    runtime: runtime.clone(),
                    replacement,
                    current_revision: revision,
                    completed: completed.clone(),
                },
                1,
            )))
            .build()
            .await
            .unwrap();
        let mut request = RunRequest::new("alice-session", "question");
        request.model = Some(ModelRef::new("personai", "main"));
        let result = agent.run(request).await;
        if switch_account {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("account changed before dispatch")
            );
            assert!(old_calls.lock().unwrap().is_empty());
            assert_eq!(*completed.lock().unwrap(), ["failed"]);
        } else {
            assert_eq!(result.unwrap().output, "bound answer");
            assert_eq!(*old_calls.lock().unwrap(), ["summary", "main"]);
            assert_eq!(*completed.lock().unwrap(), ["bound answer"]);
        }
        assert!(
            new_calls.lock().unwrap().is_empty(),
            "an existing invocation used refreshed credentials"
        );
        agent.framework().close().await;
    }
}
