use async_trait::async_trait;
use eliteforge_agent_framework::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

struct AttemptModel {
    calls: AtomicUsize,
    fail_first: bool,
    reported: bool,
    requests: Mutex<Vec<ChatRequest>>,
}
#[async_trait]
impl ChatModel for AttemptModel {
    async fn generate(
        &self,
        _: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        self.requests.lock().unwrap().push(request);
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_first && call == 0 {
            return Err(ModelError::Transient("temporary".into()));
        }
        let response = ChatResponse::text("ok");
        Ok(if self.reported {
            ChatResponse {
                usage: Usage {
                    input_tokens: 4,
                    output_tokens: 2,
                },
                ..response
            }
            .with_provider_metrics(ProviderMetrics {
                cached_input_tokens: Some(3),
                reasoning_tokens: Some(1),
            })
        } else {
            response
        })
    }
}
fn job(model: Arc<AttemptModel>) -> ChatModelJob {
    ChatModelJob {
        connection: ModelConnection::new("provider", model, vec![ModelDescriptor::new("actual")]),
        model: ModelRef::new("provider", "actual"),
        request: ChatRequest {
            messages: vec![Message::user("hello")],
            tools: vec![],
            extensions: Default::default(),
            options: GenerationOptions {
                max_output_tokens: Some(8),
                ..Default::default()
            },
        },
        limits: RunLimits {
            max_model_retries: 1,
            max_model_calls_per_step: Some(2),
            ..Default::default()
        },
        max_calls: 2,
        repair: None,
        workflow: None,
        stage: "stateless".into(),
    }
}
#[tokio::test]
async fn observed_report_distinguishes_complete_actual_unknown_and_retry_subtotal() {
    let client = ModelJobClient::new().await.unwrap();
    for (fail_first, reported, calls) in [(false, true, 1), (false, false, 1), (true, true, 2)] {
        let model = Arc::new(AttemptModel {
            calls: AtomicUsize::new(0),
            fail_first,
            reported,
            requests: Mutex::new(vec![]),
        });
        let observed = client.execute_observed(job(model.clone())).await;
        assert_eq!(observed.result.unwrap().content.text(), "ok");
        assert_eq!(observed.report.calls, calls);
        assert_eq!(observed.report.attempts, calls);
        assert_eq!(model.calls.load(Ordering::SeqCst), calls);
        assert_eq!(
            observed.report.provider_usage.is_some(),
            reported && !fail_first
        );
        assert_eq!(
            observed.report.provider_usage_subtotal,
            reported.then_some(Usage {
                input_tokens: 4,
                output_tokens: 2
            })
        );
        if fail_first || !reported {
            assert!(observed.report.ledger_usage.input_tokens > 4);
            assert!(observed.report.provider_metrics.is_none());
        } else {
            assert_eq!(
                observed
                    .report
                    .provider_metrics
                    .unwrap()
                    .cached_input_tokens,
                Some(3)
            );
        }
        assert!(
            observed
                .report
                .dispatches
                .iter()
                .all(|report| report.model == ModelRef::new("provider", "actual"))
        );
    }
    assert!(client.close_observed().await.is_success());
}
struct Repair;
#[async_trait]
impl ModelJobRepair for Repair {
    async fn validate(
        &self,
        _: &ChatResponse,
        attempt: usize,
    ) -> Result<Option<ChatRequest>, AgentError> {
        Ok((attempt == 1).then(|| ChatRequest {
            messages: vec![Message::user("repair")],
            tools: vec![],
            options: Default::default(),
            extensions: Default::default(),
        }))
    }
}
#[tokio::test]
async fn repair_cannot_reset_inherited_tree_budget_or_provider_usage_unknown() {
    let model = Arc::new(AttemptModel {
        calls: AtomicUsize::new(0),
        fail_first: false,
        reported: false,
        requests: Mutex::new(vec![]),
    });
    let client = ModelJobClient::new().await.unwrap();
    let mut job = job(model.clone());
    job.repair = Some(Arc::new(Repair));
    job.request
        .extensions
        .insert(ContinuationBudget::new(1, 4, 0, 0, 0, 0).unwrap());
    let observed = client.execute_observed(job).await;
    assert!(
        observed
            .result
            .unwrap_err()
            .to_string()
            .contains("call budget")
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    assert_eq!(observed.report.calls, 1);
    assert!(observed.report.ledger_usage.input_tokens > 0);
    assert!(observed.report.provider_usage.is_none());
    assert!(client.close_observed().await.is_success());
}
