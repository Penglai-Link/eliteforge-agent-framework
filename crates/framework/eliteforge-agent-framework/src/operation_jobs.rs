use crate::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

pub struct ModelOperationResult<T> {
    pub response: T,
    pub usage: Usage,
    pub calls: usize,
}
enum Input {
    Embed(EmbeddingModelService, EmbeddingRequest),
    Rerank(RerankModelService, RerankRequest),
}
enum Output {
    Embed(EmbeddingResponse),
    Rerank(RerankResponse),
}
struct OperationJob {
    input: Input,
    output: Mutex<Option<Output>>,
    max_calls: usize,
    report: Arc<crate::model_jobs::ModelReportState>,
    calls: AtomicUsize,
}
impl ModelJobClient {
    pub async fn embed(
        &self,
        service: EmbeddingModelService,
        request: EmbeddingRequest,
        limits: RunLimits,
        max_calls: usize,
    ) -> Result<EmbeddingResponse, AgentError> {
        match self
            .operation(Input::Embed(service, request), limits, max_calls)
            .await?
            .0
        {
            Output::Embed(response) => Ok(response),
            _ => unreachable!(),
        }
    }
    pub async fn rerank(
        &self,
        service: RerankModelService,
        request: RerankRequest,
        limits: RunLimits,
        max_calls: usize,
    ) -> Result<RerankResponse, AgentError> {
        match self
            .operation(Input::Rerank(service, request), limits, max_calls)
            .await?
            .0
        {
            Output::Rerank(response) => Ok(response),
            _ => unreachable!(),
        }
    }
    pub async fn rerank_observed(
        &self,
        service: RerankModelService,
        request: RerankRequest,
        limits: RunLimits,
        max_calls: usize,
    ) -> Result<ModelOperationResult<RerankResponse>, AgentError> {
        let (output, result) = self
            .operation(Input::Rerank(service, request), limits, max_calls)
            .await?;
        match output {
            Output::Rerank(response) => Ok(ModelOperationResult {
                response,
                usage: result.usage,
                calls: result.steps,
            }),
            _ => unreachable!(),
        }
    }
    pub async fn embed_observed(
        &self,
        service: EmbeddingModelService,
        request: EmbeddingRequest,
        limits: RunLimits,
        max_calls: usize,
    ) -> ObservedModelResult<EmbeddingResponse> {
        self.operation_reported(Input::Embed(service, request), limits, max_calls)
            .await
            .map(|output| match output {
                Output::Embed(response) => response,
                _ => unreachable!(),
            })
    }
    pub async fn rerank_reported(
        &self,
        service: RerankModelService,
        request: RerankRequest,
        limits: RunLimits,
        max_calls: usize,
    ) -> ObservedModelResult<RerankResponse> {
        self.operation_reported(Input::Rerank(service, request), limits, max_calls)
            .await
            .map(|output| match output {
                Output::Rerank(response) => response,
                _ => unreachable!(),
            })
    }
    async fn operation(
        &self,
        input: Input,
        limits: RunLimits,
        max_calls: usize,
    ) -> Result<(Output, RunResult), AgentError> {
        let observed = self.operation_reported(input, limits, max_calls).await;
        let report = observed.report;
        let output = observed.result?;
        Ok((
            output,
            RunResult {
                usage: report.ledger_usage,
                steps: report.attempts,
                output: String::new(),
                content: Content::default(),
                structured: None,
                interactions: vec![],
                status: RunStatus::Completed,
                tool_calls: 0,
                external_session: None,
            },
        ))
    }
    async fn operation_reported(
        &self,
        input: Input,
        limits: RunLimits,
        max_calls: usize,
    ) -> ObservedModelResult<Output> {
        if max_calls == 0 {
            return ObservedModelResult {
                result: Err(AgentError::InvalidConfiguration(
                    "model operation call budget must be positive".into(),
                )),
                report: ModelExecutionReport::default(),
            };
        }
        let job = Arc::new(OperationJob {
            input,
            output: Mutex::new(None),
            max_calls,
            report: Arc::new(crate::model_jobs::ModelReportState::default()),
            calls: AtomicUsize::new(0),
        });
        let mut request = RunRequest::new(RunId::new_v4().to_string(), "model operation");
        request.limits = limits;
        request.extensions.insert(job.clone());
        let result = self.agent.run(request).await.and_then(|_| {
            job.output
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .ok_or_else(|| AgentError::Run("model operation produced no output".into()))
        });
        ObservedModelResult {
            result,
            report: job.report.snapshot(job.calls.load(Ordering::Acquire)),
        }
    }
}
pub(super) async fn run_operation(context: &RunContext) -> Result<Option<RunResult>, AgentError> {
    let Some(job) = context.extensions.get::<Arc<OperationJob>>() else {
        return Ok(None);
    };
    let observed_context = job.report.attach(context);
    let context = &observed_context;
    let mut update = ModelRuntimeUpdate::new("job", 0, vec![]);
    match &job.input {
        Input::Embed(service, _) => update.embedding.push(service.clone()),
        Input::Rerank(service, _) => update.rerank.push(service.clone()),
    }
    let operations = ModelClient::from_runtime(ModelRuntime::new(update)?)
        .with_options(ModelInvocationOptions::for_run(context, context.limits()));
    let mut calls = 0;
    let mut refreshed = false;
    loop {
        context.check_cancelled()?;
        if calls >= job.max_calls {
            return Err(AgentError::Run(
                "model operation call budget exhausted".into(),
            ));
        }
        calls += 1;
        job.calls.store(calls, Ordering::Release);
        let result = match &job.input {
            Input::Embed(_, request) => operations.embed(request.clone()).await.map(Output::Embed),
            Input::Rerank(_, request) => {
                operations.rerank(request.clone()).await.map(Output::Rerank)
            }
        };
        match result {
            Ok(output) => {
                if let Output::Embed(response) = &output {
                    job.report
                        .operation_usage(response.input_tokens.map(|input_tokens| Usage {
                            input_tokens,
                            output_tokens: 0,
                        }));
                }
                *job.output.lock().unwrap_or_else(|e| e.into_inner()) = Some(output);
                return Ok(Some(RunResult {
                    output: String::new(),
                    content: Content::default(),
                    structured: None,
                    interactions: vec![],
                    status: RunStatus::Completed,
                    usage: context.usage(),
                    steps: calls,
                    tool_calls: 0,
                    external_session: None,
                }));
            }
            Err(AgentError::Model(ModelError::Provider(ref error)))
                if error.category == ProviderErrorCategory::Authentication
                    && !refreshed
                    && calls < job.max_calls =>
            {
                refreshed = true;
                let recovery = async {
                    match &job.input {
                        Input::Embed(service, _) => {
                            service
                                .implementation
                                .recover_authentication(&ModelError::Provider(error.clone()))
                                .await
                        }
                        Input::Rerank(service, _) => {
                            service
                                .implementation
                                .recover_authentication(&ModelError::Provider(error.clone()))
                                .await
                        }
                    }
                };
                let recovered = tokio::select! { biased; _ = context.cancelled() => return Err(AgentError::Cancelled), result = tokio::time::timeout(context.limits().model_timeout, recovery) => result.unwrap_or_else(|_|Err(ModelError::Fatal("credential refresh timed out".into()))) };
                let recovered = match recovered {
                    Ok(value) => value,
                    Err(refresh_error) => {
                        let failure = ModelError::Provider(error.clone());
                        match &job.input {
                            Input::Embed(service, _) => service.implementation.on_failure(&failure),
                            Input::Rerank(service, _) => {
                                service.implementation.on_failure(&failure)
                            }
                        }
                        return Err(refresh_error.into());
                    }
                };
                if !recovered {
                    let failure = ModelError::Provider(error.clone());
                    match &job.input {
                        Input::Embed(service, _) => service.implementation.on_failure(&failure),
                        Input::Rerank(service, _) => service.implementation.on_failure(&failure),
                    }
                    return Err(failure.into());
                }
            }
            Err(AgentError::Model(ref error))
                if error.is_transient()
                    && calls <= context.limits().max_model_retries
                    && calls < job.max_calls => {}
            Err(error) => {
                if let AgentError::Model(model_error) = error.primary() {
                    match &job.input {
                        Input::Embed(service, _) => service.implementation.on_failure(model_error),
                        Input::Rerank(service, _) => service.implementation.on_failure(model_error),
                    }
                }
                return Err(error);
            }
        }
        context
            .emit(AgentEvent::ModelRetry {
                step: 0,
                attempt: calls,
            })
            .await;
    }
}
