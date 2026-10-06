use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_test_support::TestBuilder;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Embedder(AtomicUsize);
#[async_trait]
impl EmbeddingModel for Embedder {
    async fn embed(&self, _: &str, _: EmbeddingRequest) -> Result<EmbeddingResponse, ModelError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(EmbeddingResponse {
            provider_model: None,
            model: ModelRef::new("authorized", "embed"),
            vectors: vec![EmbeddingVector {
                index: 0,
                values: vec![1.0],
            }],
            input_tokens: Some(4),
        })
    }
}
struct Register(Arc<Embedder>);
#[async_trait]
impl Plugin for Register {
    fn id(&self) -> &str {
        "embedding-fixture"
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        context.contribute(EmbeddingModelService::new(
            ModelRef::new("authorized", "embed"),
            self.0.clone(),
        ))
    }
}
struct Probe {
    stop: bool,
}
#[async_trait]
impl AgentLoop for Probe {
    async fn run(
        &self,
        _: &RunRequest,
        run: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let options = ModelInvocationOptions::for_run(run, run.limits());
        if !self.stop {
            options.cancellation.cancel();
        }
        let operations = services.operations.with_options(options);
        if self.stop {
            operations.stop();
        }
        let mut request = EmbeddingRequest::texts(["cost"]);
        request.choice = ModelChoice::model(ModelRef::new("authorized", "embed"));
        let error = operations.embed(request).await.unwrap_err();
        assert!(
            matches!(error, AgentError::Closed | AgentError::Cancelled),
            "{error}"
        );
        assert_eq!(run.usage(), Usage::default());
        assert_eq!(
            run.extensions.get::<ContinuationBudget>().unwrap().calls(),
            0
        );
        assert_eq!(
            run.extensions
                .get::<ModelDispatchCounter>()
                .unwrap()
                .calls(),
            0
        );
        Ok(RunResult {
            output: "done".into(),
            content: "done".into(),
            structured: None,
            interactions: vec![],
            status: RunStatus::Completed,
            usage: run.usage(),
            steps: 0,
            tool_calls: 0,
            external_session: None,
        })
    }
}
#[tokio::test]
async fn closed_or_cancelled_typed_preflight_spends_neither_tokens_nor_tree_call_slots() {
    for stop in [false, true] {
        let adapter = Arc::new(Embedder(AtomicUsize::new(0)));
        let agent = TestBuilder::default()
            .plugin(Register(adapter.clone()))
            .plugin(AgentLoopPlugin::new(Probe { stop }))
            .build()
            .await
            .unwrap();
        let mut request = RunRequest::new("admission", "go");
        request
            .extensions
            .insert(ContinuationBudget::new(1, 4, 0, 0, 0, 0).unwrap());
        assert_eq!(agent.run(request).await.unwrap().output, "done");
        assert_eq!(adapter.0.load(Ordering::SeqCst), 0);
        assert!(agent.close().await.is_success());
    }
}

#[tokio::test]
async fn observed_embedding_keeps_actual_binding_identity_and_complete_provider_usage() {
    let model = Arc::new(Embedder(AtomicUsize::new(0)));
    let reference = ModelRef::new("authorized", "embed");
    let mut request = EmbeddingRequest::texts(["cost"]);
    request.choice = ModelChoice::model(reference.clone());
    let mut registration = ModelRuntimeUpdate::new("authorized", 1, vec![]);
    registration
        .embedding
        .push(EmbeddingModelService::new(reference.clone(), model.clone()));
    let client = ModelClient::new(registration).unwrap();
    let limits = RunLimits {
        max_model_calls_per_step: Some(1),
        ..Default::default()
    };
    let observed = client.embed_observed(request, &limits).await;
    assert_eq!(observed.result.unwrap().model, reference);
    assert_eq!(observed.report.selected, vec![reference]);
    assert_eq!(observed.report.calls, 1);
    assert_eq!(
        observed.report.provider_usage,
        Some(Usage {
            input_tokens: 4,
            output_tokens: 0
        })
    );
    assert_eq!(
        observed.report.ledger_usage,
        Usage {
            input_tokens: 4,
            output_tokens: 0
        }
    );
    assert_eq!(model.0.load(Ordering::SeqCst), 1);
    assert!(client.close_observed().await.is_success());
}
