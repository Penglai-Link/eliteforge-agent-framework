//! Stateless host model work uses the same Agent lifecycle and ModelInvoker as
//! agent turns. A repair is a new counted call in the original job budget.
use crate::*;
#[path = "model_job_executor.rs"]
mod executor;
#[path = "model_observation.rs"]
mod observation;
use async_trait::async_trait;
pub(crate) use observation::ModelReportState;
pub use observation::{ModelExecutionReport, ObservedModelResult};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

#[async_trait]
pub trait ModelJobRepair: Send + Sync {
    /// Product validation can supply another request. None accepts the result.
    /// The Framework owns the loop, shared budget, usage and cancellation.
    async fn validate(
        &self,
        response: &ChatResponse,
        attempt: usize,
    ) -> Result<Option<ChatRequest>, AgentError>;
}

pub struct ChatModelJob {
    pub workflow: Option<Arc<dyn ModelWorkflow>>,
    pub connection: ModelConnection,
    pub model: ModelRef,
    pub request: ChatRequest,
    pub limits: RunLimits,
    pub stage: String,
    pub max_calls: usize,
    pub repair: Option<Arc<dyn ModelJobRepair>>,
}
struct JobState {
    job: ChatModelJob,
    response: Mutex<Option<ChatResponse>>,
    calls: Arc<AtomicUsize>,
    report: Arc<ModelReportState>,
}

/// Shared, closeable job host. Closing rejects admission and cancels/drains
/// every active job, including detached host summary jobs awaiting a result.
#[derive(Clone)]
pub struct ModelJobClient {
    pub(crate) agent: Agent,
}
impl ModelJobClient {
    pub async fn new() -> Result<Self, AgentError> {
        Ok(Self {
            agent: AgentBuilder::new()
                .default_models(false)
                .plugin(AgentLoopPlugin::new(ModelJobLoop))
                .build()
                .await?,
        })
    }
    pub async fn execute(&self, job: ChatModelJob) -> Result<ChatResponse, AgentError> {
        self.execute_observed(job).await.into_result()
    }
    pub fn start(&self, job: ChatModelJob) -> Result<ModelJobHandle, AgentError> {
        if job.max_calls == 0 {
            return Err(AgentError::InvalidConfiguration(
                "model job call budget must be positive".into(),
            ));
        }
        let mut request = RunRequest::new(RunId::new_v4().to_string(), "model job");
        request.limits = job.limits.clone();
        request.model = Some(job.model.clone());
        request.model_stage = job.stage.clone();
        request.extensions = job.request.extensions.clone();
        let state = Arc::new(JobState {
            job,
            response: Mutex::new(None),
            calls: Arc::new(AtomicUsize::new(0)),
            report: Arc::new(ModelReportState::default()),
        });
        request.extensions.insert(state.clone());
        let handle = self.agent.start(request)?;
        Ok(ModelJobHandle {
            handle: Some(handle),
            state,
            agent: self.agent.clone(),
        })
    }
    pub async fn execute_observed(&self, job: ChatModelJob) -> ObservedModelResult<ChatResponse> {
        match self.start(job) {
            Ok(mut handle) => self.wait_observed(&mut handle).await,
            Err(error) => ObservedModelResult {
                result: Err(error),
                report: ModelExecutionReport::default(),
            },
        }
    }
    pub async fn execute_child_observed(
        &self,
        job: ChatModelJob,
        parent: &RunContext,
    ) -> ObservedModelResult<ChatResponse> {
        if job.max_calls == 0 {
            return ObservedModelResult {
                result: Err(AgentError::InvalidConfiguration(
                    "model job call budget must be positive".into(),
                )),
                report: ModelExecutionReport::default(),
            };
        }
        let mut request = RunRequest::new(RunId::new_v4().to_string(), "model job");
        request.limits = job.limits.clone();
        request.model = Some(job.model.clone());
        request.model_stage = job.stage.clone();
        request.extensions = job.request.extensions.clone();
        let state = Arc::new(JobState {
            job,
            response: Mutex::new(None),
            calls: Arc::new(AtomicUsize::new(0)),
            report: Arc::new(ModelReportState::default()),
        });
        request.extensions.insert(state.clone());
        let result = self.agent.run_child(request, parent).await;
        Self::observed_response(&state, result.map(|_| ()))
    }
    fn observed_response(
        state: &JobState,
        result: Result<(), AgentError>,
    ) -> ObservedModelResult<ChatResponse> {
        let result = result.and_then(|()| {
            state
                .response
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .ok_or_else(|| AgentError::Run("model job completed without response".into()))
        });
        ObservedModelResult {
            result,
            report: state.report.snapshot(state.calls.load(Ordering::Acquire)),
        }
    }
    pub async fn wait(&self, handle: &mut ModelJobHandle) -> Result<ChatResponse, AgentError> {
        self.wait_observed(handle).await.into_result()
    }
    pub async fn wait_observed(
        &self,
        handle: &mut ModelJobHandle,
    ) -> ObservedModelResult<ChatResponse> {
        let result = match handle.handle.take() {
            Some(run) => self.agent.wait(run).await.map(|_| ()),
            None => Err(AgentError::InvalidConfiguration(
                "model job result already taken".into(),
            )),
        };
        Self::observed_response(&handle.state, result)
    }
    pub async fn close_observed(&self) -> eliteforge_agent_plugin_core::ShutdownReport {
        self.agent.close().await
    }
    pub async fn close(&self) {
        let _ = self.close_observed().await;
    }
    pub fn is_closed(&self) -> bool {
        self.agent.is_closed()
    }
}
pub struct ModelJobHandle {
    handle: Option<RunHandle>,
    state: Arc<JobState>,
    agent: Agent,
}
impl ModelJobHandle {
    pub fn calls(&self) -> usize {
        self.state.calls.load(Ordering::Acquire)
    }
    pub fn control(&self) -> Result<ModelJobControl, AgentError> {
        Ok(ModelJobControl {
            agent: self.agent.clone(),
            control: self.agent.control(
                self.handle
                    .as_ref()
                    .ok_or_else(|| AgentError::Run("model job already awaited".into()))?,
            )?,
        })
    }
    pub fn cancel(&self) {
        if let Some(handle) = &self.handle {
            let _ = self.agent.cancel(handle);
        }
    }
    pub fn events(
        &mut self,
    ) -> Result<tokio::sync::broadcast::Receiver<EventEnvelope>, AgentError> {
        self.agent.events(
            self.handle
                .as_mut()
                .ok_or_else(|| AgentError::Run("model job already awaited".into()))?,
        )
    }
}

struct CountedModel {
    inner: Arc<dyn ChatModel>,
    calls: Arc<AtomicUsize>,
    max: usize,
    report: Arc<ModelReportState>,
}
#[async_trait]
impl ChatModel for CountedModel {
    fn on_failure(&self, extensions: &Extensions, error: &ModelError) {
        self.inner.on_failure(extensions, error);
    }
    async fn generate(
        &self,
        model: &str,
        request: ChatRequest,
        stream: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        self.calls
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |calls| {
                (calls < self.max).then(|| calls + 1)
            })
            .map_err(|_| ModelError::Fatal("model job call budget exhausted".into()))?;
        let response = self.inner.generate(model, request, stream).await?;
        self.report.response(&response);
        Ok(response)
    }
    async fn recover_authentication(
        &self,
        extensions: &Extensions,
        error: &ModelError,
    ) -> Result<bool, ModelError> {
        if self.calls.load(Ordering::Acquire) >= self.max {
            return Ok(false);
        }
        self.inner.recover_authentication(extensions, error).await
    }
}

struct ModelJobLoop;
#[async_trait]
impl AgentLoop for ModelJobLoop {
    fn requires_conversations(&self) -> bool {
        false
    }
    fn supports_steering(&self) -> bool {
        false
    }
    fn cooperative_cancellation(&self) -> bool {
        true
    }
    async fn run(
        &self,
        _: &RunRequest,
        context: &RunContext,
        _: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        if let Some(result) = crate::operation_jobs::run_operation(context).await? {
            return Ok(result);
        }
        let state = context
            .extensions
            .get::<Arc<JobState>>()
            .ok_or_else(|| AgentError::InvalidConfiguration("model job input missing".into()))?;
        let context = state.report.attach(context);
        let context = &context;
        // The native child may narrow its caller's limits. A workflow and its
        // repairs share this gate even when they supply looser ModelCall limits.
        let max_calls = context
            .limits()
            .max_model_calls_per_step
            .map_or(state.job.max_calls, |limit| limit.min(state.job.max_calls));
        let mut connection = state.job.connection.clone();
        connection.adapter = Arc::new(CountedModel {
            inner: connection.adapter.clone(),
            calls: state.calls.clone(),
            max: max_calls,
            report: state.report.clone(),
        });
        for binding in connection.bindings.values_mut() {
            binding.adapter = Arc::new(CountedModel {
                inner: binding.adapter.clone(),
                calls: state.calls.clone(),
                max: max_calls,
                report: state.report.clone(),
            });
        }
        let invoker = ModelInvoker::new(
            ModelCatalog::new(vec![connection])?,
            Some(Arc::new(FixedModelStrategy(state.job.model.clone()))),
        )?;
        let mut context = context.clone();
        let scope = ModelExecutionScope::new(
            ModelRuntime::new(ModelRuntimeUpdate::new(
                "job",
                0,
                vec![invoker.catalog().connections[&state.job.model.connection].clone()],
            ))?,
            Arc::new(FixedModelStrategy(state.job.model.clone())),
        )?;
        context.extensions.insert(scope);
        let context = &context;
        if let Some(workflow) = &state.job.workflow {
            let executor = executor::WorkflowExecutor::new(&invoker, state.as_ref(), max_calls);
            let response = workflow
                .run(context, &executor, context.limits(), &state.job.model)
                .await?;
            let result = RunResult {
                output: response.content.text(),
                content: response.content.clone(),
                structured: response.structured.clone(),
                interactions: vec![],
                status: RunStatus::Completed,
                usage: context.usage(),
                steps: state.calls.load(Ordering::Acquire),
                tool_calls: 0,
                external_session: None,
            };
            *state.response.lock().unwrap_or_else(|e| e.into_inner()) = Some(response);
            return Ok(result);
        }
        let mut request = state.job.request.clone();
        let mut repairs = 0;
        loop {
            context.check_cancelled()?;
            let remaining = max_calls.saturating_sub(state.calls.load(Ordering::Acquire));
            if remaining == 0 {
                return Err(AgentError::Run("model job call budget exhausted".into()));
            }
            let mut invocation_limits = context.limits().clone();
            invocation_limits.max_model_calls_per_step = Some(remaining);
            let mut invocation_context = context.clone();
            invocation_context.extensions = request.extensions.clone();
            // Repair may change application input, never native parent admission.
            if let Some(budget) = context.extensions.get::<ContinuationBudget>() {
                invocation_context.extensions.insert((*budget).clone());
            }
            if let Some(counter) = context.extensions.get::<ModelDispatchCounter>() {
                invocation_context.extensions.insert((*counter).clone());
            }
            invocation_context.extensions.insert(
                context
                    .extensions
                    .get::<ModelExecutionScope>()
                    .expect("job binding scope")
                    .as_ref()
                    .clone(),
            );
            let mut transcript = TranscriptProjection::new(request.messages.clone());
            let response = invoker
                .invoke(
                    ModelCall {
                        stage: &state.job.stage,
                        step: repairs,
                        instructions: &[],
                        tools: &request.tools,
                        limits: &invocation_limits,
                        options: &request.options,
                    },
                    &mut transcript,
                    &invocation_context,
                )
                .await?;
            if let Some(repair) = &state.job.repair {
                let next = tokio::select! { biased; _ = context.cancelled() => return Err(AgentError::Cancelled), next = repair.validate(&response, repairs + 1) => next? };
                if let Some(next) = next {
                    request = next;
                    repairs += 1;
                    continue;
                }
            }
            let result = RunResult {
                output: response.content.text(),
                content: response.content.clone(),
                structured: response.structured.clone(),
                interactions: vec![],
                status: RunStatus::Completed,
                usage: context.usage(),
                steps: state.calls.load(Ordering::Acquire),
                tool_calls: 0,
                external_session: None,
            };
            *state.response.lock().unwrap_or_else(|e| e.into_inner()) = Some(response);
            return Ok(result);
        }
    }
}

#[derive(Clone)]
pub struct ModelJobControl {
    agent: Agent,
    control: RunControl,
}
impl ModelJobControl {
    pub fn cancel(&self) -> Result<(), AgentError> {
        self.agent.cancel(&self.control)
    }
}

/// Lazy job host with serialized first initialization and close admission.
/// Closing before first use permanently prevents creating an executor.
#[derive(Default)]
pub struct LazyModelJobClient {
    client: tokio::sync::OnceCell<ModelJobClient>,
    admission: tokio::sync::Mutex<()>,
    closed: std::sync::atomic::AtomicBool,
}
impl LazyModelJobClient {
    pub async fn get(&self) -> Result<&ModelJobClient, AgentError> {
        let _admission = self.admission.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return Err(AgentError::Closed);
        }
        self.client.get_or_try_init(ModelJobClient::new).await
    }
    pub async fn close_observed(&self) -> eliteforge_agent_plugin_core::ShutdownReport {
        let _admission = self.admission.lock().await;
        self.closed.store(true, Ordering::Release);
        if let Some(client) = self.client.get() {
            return client.close_observed().await;
        }
        eliteforge_agent_plugin_core::ShutdownReport::default()
    }
    pub async fn close(&self) {
        let _ = self.close_observed().await;
    }
}
