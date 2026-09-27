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
    agent.close().await;
}
#[tokio::test]
async fn unchanged_source_is_rejected_before_a_second_summary_call() {
    let (agent, model, _) = setup(true, false).await;
    let error = agent.run(request()).await.unwrap_err();
    assert!(error.to_string().contains("no source progress"), "{error}");
    assert_eq!(*model.0.lock().unwrap(), ["summary"]);
    agent.close().await;
}
#[tokio::test]
async fn failed_summary_cannot_be_silently_accepted_by_completion_hook() {
    let (agent, model, completed) = setup(false, true).await;
    assert!(agent.run(request()).await.is_err());
    assert_eq!(*model.0.lock().unwrap(), ["failure"]);
    assert_eq!(*completed.lock().unwrap(), ["failed"]);
    agent.close().await;
}

#[tokio::test]
async fn auxiliary_calls_reserve_the_last_dispatch_for_the_answer() {
    let (agent, model, completed) = setup(false, false).await;
    let mut request = request();
    request.limits.max_model_calls_per_step = Some(1);
    assert!(agent.run(request).await.is_err());
    assert!(model.0.lock().unwrap().is_empty());
    assert_eq!(*completed.lock().unwrap(), ["failed"]);
    agent.close().await;
}
