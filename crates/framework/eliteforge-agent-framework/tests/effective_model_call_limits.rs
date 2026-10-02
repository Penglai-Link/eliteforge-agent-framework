use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_test_support::TestBuilder;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
struct Provider {
    calls: AtomicUsize,
    fail: bool,
}
#[async_trait]
impl ChatModel for Provider {
    async fn generate(
        &self,
        _: &str,
        _: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            Err(ModelError::Transient("retry".into()))
        } else {
            Ok(ChatResponse {
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
                ..ChatResponse::text("summary")
            })
        }
    }
}
fn connection(provider: Arc<Provider>) -> ModelConnection {
    ModelConnection::new("actual", provider, vec![ModelDescriptor::new("model")])
}
fn input() -> ChatRequest {
    ChatRequest {
        messages: vec![Message::user("input")],
        tools: vec![],
        options: GenerationOptions {
            max_output_tokens: Some(8),
            ..Default::default()
        },
        extensions: Default::default(),
    }
}
struct Done;
#[async_trait]
impl ContextReductionCompletion for Done {
    async fn complete(&self, result: Result<ChatResponse, AgentError>) -> Result<(), AgentError> {
        result.map(|_| ())
    }
}
struct Reduction;
#[async_trait]
impl ContextPreparationPolicy for Reduction {
    async fn prepare(
        &self,
        input: &ContextAssemblyInput<'_>,
        _: usize,
    ) -> Result<ContextPreparation, AgentError> {
        Ok(ContextPreparation::Reduce(ContextReductionJob {
            source_revision: "source".into(),
            model: ModelRef::new("actual", "model"),
            request: crate::input(),
            stage: "summary".into(),
            limits: input.limits.clone(),
            completion: Arc::new(Done),
            workflow: None,
        }))
    }
}
struct GenericLoop {
    provider: Arc<Provider>,
    aux: bool,
    usage: Arc<Mutex<Option<Usage>>>,
}
#[async_trait]
impl AgentLoop for GenericLoop {
    async fn run(
        &self,
        _: &RunRequest,
        run: &RunContext,
        _: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let mut invoker = ModelInvoker::new(
            ModelCatalog::new(vec![connection(self.provider.clone())])?,
            Some(Arc::new(FixedModelStrategy(ModelRef::new(
                "actual", "model",
            )))),
        )?;
        if self.aux {
            invoker =
                invoker.with_assembler(Arc::new(PreparingContextAssembler::new(Reduction, 2)));
        }
        let mut loose = run.limits().clone();
        loose.max_model_calls_per_step = None;
        loose.max_model_retries = 4;
        let mut memory = TranscriptProjection::new(vec![Message::user("input")]);
        let options = input().options;
        let result = invoker
            .invoke(
                ModelCall {
                    stage: "main",
                    step: 0,
                    instructions: &[],
                    tools: &[],
                    limits: &loose,
                    options: &options,
                },
                &mut memory,
                run,
            )
            .await;
        *self.usage.lock().unwrap() = Some(run.usage());
        Err(result.unwrap_err())
    }
}
#[tokio::test]
async fn loose_model_call_cannot_expand_effective_retry_or_auxiliary_answer_reserve() {
    for aux in [false, true] {
        let provider = Arc::new(Provider {
            calls: AtomicUsize::new(0),
            fail: !aux,
        });
        let usage = Arc::new(Mutex::new(None));
        let agent = TestBuilder::default()
            .plugin(AgentLoopPlugin::new(GenericLoop {
                provider: provider.clone(),
                aux,
                usage: usage.clone(),
            }))
            .build()
            .await
            .unwrap();
        let budget = ContinuationBudget::new(4, 4, 0, 0, 0, 0).unwrap();
        let mut request = RunRequest::new("bounded", "go");
        request.limits.max_model_calls_per_step = Some(1);
        request.model = Some(ModelRef::new("actual", "model"));
        request.extensions.insert(budget.clone());
        assert!(
            agent
                .run(request)
                .await
                .unwrap_err()
                .to_string()
                .contains("call budget")
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), usize::from(!aux));
        assert_eq!(budget.calls(), usize::from(!aux));
        if aux {
            assert_eq!(*usage.lock().unwrap(), Some(Usage::default()));
        }
        assert!(agent.close().await.is_success());
    }
}
struct LooseWorkflow;
#[async_trait]
impl ModelWorkflow for LooseWorkflow {
    async fn run(
        &self,
        run: &RunContext,
        models: &dyn ChatExecutor,
        limits: &RunLimits,
        model: &ModelRef,
    ) -> Result<ChatResponse, AgentError> {
        assert_eq!(limits.max_model_calls_per_step, Some(1));
        let mut loose = limits.clone();
        loose.max_model_calls_per_step = None;
        models
            .invoke_auxiliary(model, input(), "first", &loose, run)
            .await?;
        models
            .invoke_auxiliary(model, input(), "forbidden", &loose, run)
            .await
    }
}
#[tokio::test]
async fn workflow_loose_auxiliary_does_not_create_a_phantom_dispatch_after_effective_cap() {
    let provider = Arc::new(Provider {
        calls: AtomicUsize::new(0),
        fail: false,
    });
    let budget = ContinuationBudget::new(4, 4, 0, 0, 0, 0).unwrap();
    let mut request = input();
    request.extensions.insert(budget.clone());
    let client = ModelJobClient::new().await.unwrap();
    let observed = client
        .execute_observed(ChatModelJob {
            connection: connection(provider.clone()),
            model: ModelRef::new("actual", "model"),
            request,
            limits: RunLimits {
                max_model_calls_per_step: Some(1),
                ..Default::default()
            },
            stage: "workflow".into(),
            max_calls: 3,
            workflow: Some(Arc::new(LooseWorkflow)),
            repair: None,
        })
        .await;
    assert!(
        observed
            .result
            .unwrap_err()
            .to_string()
            .contains("job call budget")
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(observed.report.calls, 1);
    assert_eq!(observed.report.attempts, 1);
    assert_eq!(budget.calls(), 1);
    assert_eq!(
        observed.report.ledger_usage,
        Usage {
            input_tokens: 1,
            output_tokens: 1
        }
    );
    assert_eq!(
        observed.report.provider_usage,
        Some(Usage {
            input_tokens: 1,
            output_tokens: 1
        })
    );
    assert!(client.close_observed().await.is_success());
}
