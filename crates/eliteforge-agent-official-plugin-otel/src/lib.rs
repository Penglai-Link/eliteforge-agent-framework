//! OpenTelemetry observation with explicit provider ownership and bounded state.
//!
//! The default plugin reads the host's global providers during installation;
//! `new` accepts explicit providers. The host owns exporter configuration,
//! flushing and shutdown. This crate never changes the global provider. Prompts, tool
//! arguments/results, model responses, sessions, credentials, and extensions
//! are not collected as payloads. Error text is omitted unless the host supplies
//! an explicit redactor. Optional identifiers remain span-only.
//!
//! Register this plugin before other middleware to enclose their execution.
//! Interceptors observe outcomes at their position in the chain; an outer
//! interceptor can still change the final result. Lifecycle hooks are separate.
//! Run/node/model/tool futures own span drop guards. Container cleanup releases
//! remaining correlation metadata. A hard runtime abort cannot
//! promise prompt export; state is bounded and abandoned spans are never marked
//! successful. Export durability and metric cardinality limits belong to the SDK.

use async_trait::async_trait;
use eliteforge_agent_core::{
    AgentError, AgentEvent, ChatRequest, ChatResponse, ContextBlockOutcome, ErrorRedactor,
    ErrorReport, EventService, EventSink, ModelError, ModelInvocationContext, ModelMiddleware,
    ModelMiddlewareService, ModelNext, OmitErrorMessages, RunContext, RunId, RunMiddleware,
    RunMiddlewareService, RunNext, RunRequest, RunResult, RunStatus, ToolError,
    ToolInvocationContext, ToolMiddleware, ToolMiddlewareService, ToolNext, ToolOutput, Usage,
    WorkflowNodeContext, WorkflowNodeInput, WorkflowNodeMiddleware, WorkflowNodeMiddlewareService,
    WorkflowNodeNext, WorkflowNodeResult,
};
use eliteforge_agent_core::{Plugin, PluginContext, PluginError};
use opentelemetry::{
    Context, KeyValue,
    metrics::{Counter, Histogram, Meter, MeterProvider},
    trace::{
        FutureExt, Link, SpanBuilder, SpanContext, SpanKind, Status, TraceContextExt, Tracer,
        TracerProvider,
    },
};
use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, Mutex},
    time::Instant,
};

const SCOPE: &str = "eliteforge-agent-official-plugin-otel";
type StartSpan = dyn Fn(SpanBuilder, &Context) -> Context + Send + Sync;

/// An explicitly propagated host span. Insert in `RunRequest.extensions` before
/// starting a run; task-local context does not automatically cross Tokio spawn.
#[derive(Clone)]
pub struct TraceParent(pub Context);

/// Observation limits never reject agent execution. At capacity, instrumentation
/// is skipped and `agent.telemetry.dropped` records an allowlisted reason.
#[derive(Clone, Debug)]
pub struct TelemetryConfig {
    pub max_active_runs: usize,
    pub max_tools_per_run: usize,
    pub max_model_links_per_run: usize,
    /// Export workflow, node, model, tool, and stage names on spans. Disabled for privacy by
    /// default. These identifiers are host-controlled and must not contain secrets.
    pub record_identifiers: bool,
    pub max_identifier_bytes: usize,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            max_active_runs: 4096,
            max_tools_per_run: 256,
            max_model_links_per_run: 256,
            record_identifiers: false,
            max_identifier_bytes: 128,
        }
    }
}

#[derive(Debug)]
pub struct TelemetryConfigError(&'static str);

impl fmt::Display for TelemetryConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for TelemetryConfigError {}

/// A reference plugin using the host's OpenTelemetry tracer and meter providers.
/// Every installation owns a distinct bounded registry, including when this
/// plugin instance is installed in several agents that share SDK providers.
pub struct TelemetryPlugin {
    id: String,
    config: TelemetryConfig,
    redactor: Arc<dyn ErrorRedactor>,
    instruments: Option<(Arc<StartSpan>, Metrics)>,
}

impl Default for TelemetryPlugin {
    /// Use global providers at Agent build time; no export occurs without an exporter.
    fn default() -> Self {
        Self {
            id: "agent.telemetry".into(),
            config: TelemetryConfig::default(),
            redactor: Arc::new(OmitErrorMessages),
            instruments: None,
        }
    }
}

impl TelemetryPlugin {
    /// Opt in to safe error text. The host owns the redaction policy; exceptions
    /// in that policy fail closed. Reports remain bounded after redaction.
    pub fn with_error_redactor(mut self, redactor: Arc<dyn ErrorRedactor>) -> Self {
        self.redactor = redactor;
        self
    }

    pub fn new<TP, MP>(
        id: impl Into<String>,
        tracer_provider: &TP,
        meter_provider: &MP,
        config: TelemetryConfig,
    ) -> Result<Self, TelemetryConfigError>
    where
        TP: TracerProvider,
        TP::Tracer: Send + Sync + 'static,
        <TP::Tracer as Tracer>::Span: Send + Sync + 'static,
        MP: MeterProvider,
    {
        if config.max_active_runs == 0
            || config.max_tools_per_run == 0
            || config.max_model_links_per_run == 0
            || config.max_identifier_bytes == 0
        {
            return Err(TelemetryConfigError("telemetry capacities must be nonzero"));
        }
        Ok(Self {
            id: id.into(),
            config,
            redactor: Arc::new(OmitErrorMessages),
            instruments: Some(instruments(tracer_provider, meter_provider)),
        })
    }
}

fn instruments<TP, MP>(tracer_provider: &TP, meter_provider: &MP) -> (Arc<StartSpan>, Metrics)
where
    TP: TracerProvider,
    TP::Tracer: Send + Sync + 'static,
    <TP::Tracer as Tracer>::Span: Send + Sync + 'static,
    MP: MeterProvider + ?Sized,
{
    let tracer = tracer_provider.tracer(SCOPE);
    (
        Arc::new(move |builder, parent| {
            parent.with_span(tracer.build_with_context(builder, parent))
        }),
        Metrics::new(meter_provider.meter(SCOPE)),
    )
}

#[async_trait]
impl Plugin for TelemetryPlugin {
    fn id(&self) -> &str {
        &self.id
    }

    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let (start, metrics) = self.instruments.clone().unwrap_or_else(|| {
            instruments(
                &opentelemetry::global::tracer_provider(),
                opentelemetry::global::meter_provider().as_ref(),
            )
        });
        let observation = Arc::new(Observation {
            config: self.config.clone(),
            redactor: self.redactor.clone(),
            start,
            metrics,
            runs: Mutex::new(HashMap::new()),
        });
        // Register rollback before contributions, including partial installation.
        let cleanup = observation.clone();
        context.on_cleanup(move || cleanup.drain());
        context.contribute(RunMiddlewareService(observation.clone()))?;
        context.contribute(ModelMiddlewareService(observation.clone()))?;
        context.contribute(WorkflowNodeMiddlewareService(observation.clone()))?;
        context.contribute(ToolMiddlewareService(observation.clone()))?;
        context.contribute(EventService(observation))
    }
}

#[derive(Clone)]
struct Metrics {
    operations: Counter<u64>,
    errors: Counter<u64>,
    cancelled: Counter<u64>,
    abandoned: Counter<u64>,
    duration: Histogram<f64>,
    tokens: Counter<u64>,
    dropped: Counter<u64>,
    context_tokens: Histogram<u64>,
}

impl Metrics {
    fn new(meter: Meter) -> Self {
        Self {
            operations: meter
                .u64_counter("agent.operation.count")
                .with_description("Finished instrumented operations")
                .build(),
            errors: meter
                .u64_counter("agent.operation.errors")
                .with_description("Operations with observed errors")
                .build(),
            cancelled: meter
                .u64_counter("agent.operation.cancelled")
                .with_description("Operations cancelled while in progress")
                .build(),
            abandoned: meter
                .u64_counter("agent.operation.abandoned")
                .with_description("Operations whose final outcome is unknown")
                .build(),
            duration: meter
                .f64_histogram("agent.operation.duration")
                .with_unit("s")
                .with_description("Instrumented operation duration")
                .build(),
            tokens: meter
                .u64_counter("agent.model.token.usage")
                .with_unit("{token}")
                .with_description("Provider-reported tokens for returned model responses")
                .build(),
            context_tokens: meter
                .u64_histogram("agent.context.estimated_tokens")
                .with_description(
                    "Estimated input tokens at assembly and dispatch; not provider billing",
                )
                .build(),
            dropped: meter
                .u64_counter("agent.telemetry.dropped")
                .with_description("Skipped observations due to registry limits or missing run")
                .build(),
        }
    }
    fn dropped(&self, operation: &'static str, reason: &'static str) {
        self.dropped.add(
            1,
            &[
                KeyValue::new("agent.operation", operation),
                KeyValue::new("agent.telemetry.reason", reason),
            ],
        );
    }
}

#[derive(Clone, Copy)]
enum Outcome {
    Success(&'static str),
    Error(&'static str),
    Cancelled,
    Unknown,
}

impl Outcome {
    fn label(self) -> &'static str {
        match self {
            Self::Success(label) => label,
            Self::Error(_) => "error",
            Self::Cancelled => "cancelled",
            Self::Unknown => "unknown",
        }
    }
}

struct SpanGuard {
    context: Context,
    run: RunContext,
    started: Instant,
    operation: &'static str,
    metrics: Metrics,
    finished: bool,
}

impl SpanGuard {
    fn finish(mut self, outcome: Outcome, usage: Option<&Usage>) {
        self.record(outcome, usage);
    }

    fn record(&mut self, outcome: Outcome, usage: Option<&Usage>) {
        if self.finished {
            return;
        }
        self.finished = true;
        let span = self.context.span();
        span.set_attribute(KeyValue::new("agent.operation.outcome", outcome.label()));
        let mut attributes = vec![
            KeyValue::new("agent.operation", self.operation),
            KeyValue::new("agent.operation.outcome", outcome.label()),
        ];
        match outcome {
            Outcome::Success(_) => span.set_status(Status::Ok),
            Outcome::Error(kind) => {
                // Status text remains allowlisted even with a custom redactor.
                // Sanitized details are recorded separately in error events.
                span.set_status(Status::error(kind));
                span.set_attribute(KeyValue::new("error.type", kind));
                attributes.push(KeyValue::new("error.type", kind));
                self.metrics.errors.add(1, &attributes);
            }
            Outcome::Cancelled => {
                span.set_attribute(KeyValue::new("agent.operation.cancelled", true));
                self.metrics.cancelled.add(1, &attributes);
            }
            Outcome::Unknown => {
                span.set_attribute(KeyValue::new("agent.operation.abandoned", true));
                self.metrics.abandoned.add(1, &attributes);
            }
        }
        // The Context is attached only across synchronous metric recording,
        // never across an await. SDK exemplars can correlate with this span.
        let _attached = self.context.clone().attach();
        self.metrics.operations.add(1, &attributes);
        self.metrics
            .duration
            .record(self.started.elapsed().as_secs_f64(), &attributes);
        if let Some(usage) = usage {
            span.set_attribute(KeyValue::new(
                "gen_ai.usage.input_tokens",
                as_i64(usage.input_tokens),
            ));
            span.set_attribute(KeyValue::new(
                "gen_ai.usage.output_tokens",
                as_i64(usage.output_tokens),
            ));
            if self.operation == "model" {
                self.metrics.tokens.add(
                    usage.input_tokens,
                    &[KeyValue::new("gen_ai.token.type", "input")],
                );
                self.metrics.tokens.add(
                    usage.output_tokens,
                    &[KeyValue::new("gen_ai.token.type", "output")],
                );
            }
        }
        span.end();
    }
}

impl Drop for SpanGuard {
    fn drop(&mut self) {
        let outcome = if self.run.is_cancelled() {
            Outcome::Cancelled
        } else {
            Outcome::Unknown
        };
        self.record(outcome, None);
    }
}

fn as_i64(value: u64) -> i64 {
    value.min(i64::MAX as u64) as i64
}

struct RunObservation {
    context: Context,
    active_tools: usize,
    model_links: HashMap<(Option<RunId>, String), SpanContext>,
}

struct Observation {
    config: TelemetryConfig,
    redactor: Arc<dyn ErrorRedactor>,
    start: Arc<StartSpan>,
    metrics: Metrics,
    runs: Mutex<HashMap<RunId, RunObservation>>,
}

impl Observation {
    fn span(
        &self,
        operation: &'static str,
        run: &RunContext,
        parent: &Context,
        mut attributes: Vec<KeyValue>,
        links: Vec<Link>,
    ) -> SpanGuard {
        let name = match operation {
            "model" => "agent.model",
            "tool" => "agent.tool",
            "node" => "agent.workflow.node",
            _ => "agent.run",
        };
        let kind = if operation == "model" {
            SpanKind::Client
        } else {
            SpanKind::Internal
        };
        attributes.extend([
            KeyValue::new("agent.run.id", run.run_id.to_string()),
            KeyValue::new("agent.execution.id", run.execution_id().to_string()),
        ]);
        if let Some(node) = run.node_execution() {
            attributes.extend([
                KeyValue::new("agent.node.invocation.id", node.invocation_id.to_string()),
                KeyValue::new("agent.node.attempt.id", node.attempt_id.to_string()),
                KeyValue::new("agent.workflow.version", i64::from(node.workflow_version)),
            ]);
            self.identifier(&mut attributes, "agent.workflow.id", &node.workflow_id);
            self.identifier(&mut attributes, "agent.node.id", &node.node_id);
            if let Some(id) = node.previous_attempt_id {
                attributes.push(KeyValue::new(
                    "agent.node.previous_attempt.id",
                    id.to_string(),
                ));
            }
            if let Some(id) = node.previous_execution_id {
                attributes.push(KeyValue::new("agent.previous_execution.id", id.to_string()));
            }
        }
        let builder = SpanBuilder::from_name(name)
            .with_kind(kind)
            .with_attributes(attributes)
            .with_links(links);
        SpanGuard {
            context: (self.start)(builder, parent),
            run: run.clone(),
            started: Instant::now(),
            operation,
            metrics: self.metrics.clone(),
            finished: false,
        }
    }

    fn report(&self, span: &SpanGuard, report: ErrorReport) {
        // Events inherit trace/span identity without a second asynchronous sink.
        if let Ok(json) = serde_json::to_string(&report) {
            span.context.span().add_event(
                "agent.error",
                vec![
                    KeyValue::new("error.type", report.kind),
                    KeyValue::new("agent.error.report", json),
                ],
            );
        }
    }

    fn identifier(&self, attributes: &mut Vec<KeyValue>, key: &'static str, value: &str) {
        if self.config.record_identifiers {
            let mut end = value.len().min(self.config.max_identifier_bytes);
            while !value.is_char_boundary(end) {
                end -= 1;
            }
            attributes.push(KeyValue::new(key, value[..end].to_owned()));
        }
    }

    fn parent(&self, run: &RunContext) -> Option<Context> {
        if let Some(parent) = node_parent(run) {
            return Some(parent);
        }
        let run_id = run.execution_id();
        self.runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&run_id)
            .map(|run| run.context.clone())
    }

    fn drain(&self) {
        self.runs.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

impl Drop for Observation {
    fn drop(&mut self) {
        self.drain();
    }
}

// Registries hold only correlation metadata. Spans are owned by the future
// they wrap, so cancellation drops them without matching before/after callbacks.
struct RunRegistryGuard<'a> {
    observation: &'a Observation,
    execution_id: RunId,
}
impl Drop for RunRegistryGuard<'_> {
    fn drop(&mut self) {
        self.observation
            .runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.execution_id);
    }
}
struct ToolRegistryGuard<'a> {
    observation: &'a Observation,
    execution_id: RunId,
}
impl Drop for ToolRegistryGuard<'_> {
    fn drop(&mut self) {
        if let Some(run) = self
            .observation
            .runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&self.execution_id)
        {
            run.active_tools = run.active_tools.saturating_sub(1);
        }
    }
}

#[async_trait]
impl RunMiddleware for Observation {
    async fn intercept(
        &self,
        request: &RunRequest,
        run: &RunContext,
        next: RunNext<'_>,
    ) -> Result<RunResult, AgentError> {
        let span = {
            let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
            if runs.len() >= self.config.max_active_runs || runs.contains_key(&run.execution_id()) {
                None
            } else {
                let parent = run
                    .extensions
                    .get::<TraceParent>()
                    .map_or_else(Context::new, |value| value.0.clone());
                let span = self.span(
                    "run",
                    run,
                    &parent,
                    vec![KeyValue::new("agent.run.resumed", request.resume.is_some())],
                    Vec::new(),
                );
                runs.insert(
                    run.execution_id(),
                    RunObservation {
                        context: span.context.clone(),
                        active_tools: 0,
                        model_links: HashMap::new(),
                    },
                );
                Some(span)
            }
        };
        let Some(span) = span else {
            self.metrics.dropped("run", "capacity_or_duplicate");
            return next.run().await;
        };
        let _registry = RunRegistryGuard {
            observation: self,
            execution_id: run.execution_id(),
        };
        let result = next.run().with_context(span.context.clone()).await;
        let outcome = match &result {
            Ok(result) => Outcome::Success(match result.status {
                RunStatus::Completed => "completed",
                RunStatus::AwaitingInput => "awaiting_input",
                RunStatus::StepLimit => "step_limit",
                RunStatus::ToolCallLimit => "tool_call_limit",
                RunStatus::RecoveryRequired => "recovery_required",
            }),
            Err(error) if matches!(error.primary(), AgentError::Cancelled) => Outcome::Cancelled,
            Err(error) => Outcome::Error(error.kind()),
        };
        if let Err(error) = &result {
            self.report(&span, error.report(self.redactor.as_ref()));
        }
        span.finish(outcome, Some(&run.usage()));
        result
    }
}

#[async_trait]
impl ModelMiddleware for Observation {
    async fn intercept(
        &self,
        request: ChatRequest,
        invocation: &ModelInvocationContext<'_>,
        next: ModelNext<'_>,
    ) -> Result<ChatResponse, ModelError> {
        let Some(parent) = self.parent(invocation.run) else {
            self.metrics.dropped("model", "missing_run");
            return next.run(request, invocation).await;
        };
        let mut attributes = vec![
            KeyValue::new("agent.model.step", as_i64(invocation.step as u64)),
            KeyValue::new("agent.model.attempt", as_i64(invocation.attempt as u64)),
            KeyValue::new(
                "agent.model.summary",
                invocation.stage == "context_compression",
            ),
        ];
        self.identifier(
            &mut attributes,
            "gen_ai.request.model",
            &invocation.model.model,
        );
        self.identifier(
            &mut attributes,
            "agent.model.connection",
            &invocation.model.connection,
        );
        self.identifier(&mut attributes, "agent.model.stage", invocation.stage);
        let span = self.span("model", invocation.run, &parent, attributes, Vec::new());
        let result = next
            .run(request, invocation)
            .with_context(span.context.clone())
            .await;
        match &result {
            Ok(response) => {
                let link = span.context.span().span_context().clone();
                let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(run) = runs.get_mut(&invocation.run.execution_id()) {
                    for call in &response.tool_calls {
                        if run.model_links.len() < self.config.max_model_links_per_run
                            || run
                                .model_links
                                .contains_key(&link_key(invocation.run, &call.id))
                        {
                            run.model_links
                                .insert(link_key(invocation.run, &call.id), link.clone());
                        } else {
                            self.metrics.dropped("model_link", "capacity_or_duplicate");
                        }
                    }
                }
                drop(runs);
                span.finish(Outcome::Success("completed"), Some(&response.usage));
            }
            Err(error) => {
                self.report(
                    &span,
                    ErrorReport::from_error(error.kind(), error, self.redactor.as_ref()),
                );
                span.finish(Outcome::Error(error.kind()), None);
            }
        }
        result
    }
}

#[async_trait]
impl ToolMiddleware for Observation {
    async fn intercept(
        &self,
        invocation: &ToolInvocationContext<'_>,
        next: ToolNext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let admission = {
            let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
            match runs.get_mut(&invocation.run.execution_id()) {
                None => Err("missing_run"),
                Some(run) if run.active_tools >= self.config.max_tools_per_run => Err("capacity"),
                Some(run) => {
                    run.active_tools += 1;
                    let links = run
                        .model_links
                        .remove(&link_key(invocation.run, &invocation.call.id))
                        .into_iter()
                        .map(|context| Link::new(context, Vec::new(), 0))
                        .collect();
                    let parent = node_parent(invocation.run).unwrap_or_else(|| run.context.clone());
                    Ok((parent, links))
                }
            }
        };
        let (parent, links) = match admission {
            Ok(value) => value,
            Err(reason) => {
                self.metrics.dropped("tool", reason);
                return next.run().await;
            }
        };
        let _registry = ToolRegistryGuard {
            observation: self,
            execution_id: invocation.run.execution_id(),
        };
        let mut attributes = Vec::new();
        self.identifier(
            &mut attributes,
            "gen_ai.tool.name",
            &invocation.descriptor.call_name,
        );
        let span = self.span("tool", invocation.run, &parent, attributes, links);
        let result = next.run().with_context(span.context.clone()).await;
        match &result {
            Err(error) => self.report(
                &span,
                ErrorReport::from_error("tool", error, self.redactor.as_ref()),
            ),
            Ok(output) if output.is_error => self.report(
                &span,
                ErrorReport::from_error(
                    "tool_result",
                    &ToolError::Failed(output.content.text()),
                    self.redactor.as_ref(),
                ),
            ),
            _ => {}
        }
        let outcome = match &result {
            Ok(output) if output.is_error => Outcome::Error("tool_result"),
            Ok(_) => Outcome::Success("completed"),
            Err(_) if invocation.run.is_cancelled() => Outcome::Cancelled,
            Err(ToolError::Indeterminate(_)) => Outcome::Error("indeterminate"),
            Err(ToolError::Failed(_)) => Outcome::Error("tool_failed"),
        };
        span.finish(outcome, None);
        result
    }
}

#[derive(Clone)]
struct NodeTrace {
    execution_id: RunId,
    attempt_id: RunId,
    context: Context,
}

// Release unconsumed model links at the end of a node attempt.
struct NodeSpanGuard<'a> {
    span: Option<SpanGuard>,
    observation: &'a Observation,
    execution_id: RunId,
    attempt_id: RunId,
}
impl NodeSpanGuard<'_> {
    fn drain_links(&self) {
        let mut runs = self
            .observation
            .runs
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(run) = runs.get_mut(&self.execution_id) {
            run.model_links
                .retain(|(attempt, _), _| *attempt != Some(self.attempt_id));
        }
    }
    fn finish(mut self, outcome: Outcome) {
        self.drain_links();
        self.span
            .take()
            .expect("active node span")
            .finish(outcome, None);
    }
}
impl Drop for NodeSpanGuard<'_> {
    fn drop(&mut self) {
        self.drain_links();
        // Nested tool futures have already been dropped before this parent guard.
    }
}
fn node_parent(run: &RunContext) -> Option<Context> {
    let node = run.node_execution()?;
    let trace = run.extensions.get::<NodeTrace>()?;
    (trace.execution_id == run.execution_id() && trace.attempt_id == node.attempt_id)
        .then(|| trace.context.clone())
}
fn link_key(run: &RunContext, call_id: &str) -> (Option<RunId>, String) {
    (
        run.node_execution().map(|node| node.attempt_id),
        call_id.to_owned(),
    )
}

#[async_trait]
impl WorkflowNodeMiddleware for Observation {
    async fn intercept(
        &self,
        input: WorkflowNodeInput,
        context: &WorkflowNodeContext,
        next: WorkflowNodeNext<'_>,
    ) -> Result<WorkflowNodeResult, AgentError> {
        let Some(parent) = self.parent(&context.run) else {
            self.metrics.dropped("node", "missing_run");
            return next.run(input, context).await;
        };
        let span = self.span("node", &context.run, &parent, Vec::new(), Vec::new());
        let mut context = context.clone();
        let identity = context
            .run
            .node_execution()
            .expect("admitted node identity");
        context.run.extensions.insert(NodeTrace {
            execution_id: context.run.execution_id(),
            attempt_id: identity.attempt_id,
            context: span.context.clone(),
        });
        let span = NodeSpanGuard {
            execution_id: context.run.execution_id(),
            attempt_id: context
                .run
                .node_execution()
                .expect("node identity")
                .attempt_id,
            span: Some(span),
            observation: self,
        };
        let result = next
            .run(input, &context)
            .with_context(
                span.span
                    .as_ref()
                    .expect("active node span")
                    .context
                    .clone(),
            )
            .await;
        let outcome = match &result {
            Ok(result) => Outcome::Success(match result.status {
                RunStatus::Completed => "completed",
                RunStatus::AwaitingInput => "awaiting_input",
                RunStatus::RecoveryRequired => "recovery_required",
                RunStatus::StepLimit => "step_limit",
                RunStatus::ToolCallLimit => "tool_call_limit",
            }),
            Err(error) => {
                self.report(
                    span.span.as_ref().expect("active node span"),
                    error.report(self.redactor.as_ref()),
                );
                if matches!(error.primary(), AgentError::Cancelled) {
                    Outcome::Cancelled
                } else {
                    Outcome::Error(error.kind())
                }
            }
        };
        span.finish(outcome);
        result
    }
}

#[async_trait]
impl EventSink for Observation {
    fn on_diagnostic(&self, run: &RunContext, event: &AgentEvent) {
        let Some(parent) = self.parent(run) else {
            return;
        };
        match event {
            AgentEvent::ContextAssembled(report) => {
                let span = parent.span();
                span.add_event(
                    "agent.context.assembled",
                    vec![
                        KeyValue::new(
                            "agent.context.input_capacity",
                            as_i64(report.input_capacity as u64),
                        ),
                        KeyValue::new(
                            "agent.context.protocol_tokens",
                            as_i64(report.protocol_tokens as u64),
                        ),
                        KeyValue::new(
                            "agent.context.estimated_input_tokens",
                            as_i64(report.estimated_input_tokens as u64),
                        ),
                        KeyValue::new(
                            "agent.context.block_count",
                            as_i64(report.blocks.len() as u64),
                        ),
                    ],
                );
                self.metrics.context_tokens.record(
                    report.estimated_input_tokens as u64,
                    &[KeyValue::new("agent.context.phase", "assembled")],
                );
                for block in &report.blocks {
                    let (outcome, reason) = match &block.outcome {
                        ContextBlockOutcome::Kept => ("kept", None),
                        ContextBlockOutcome::Reduced => ("reduced", None),
                        ContextBlockOutcome::Omitted { reason } => ("omitted", Some(reason)),
                        ContextBlockOutcome::Failed { reason } => ("failed", Some(reason)),
                    };
                    let mut attributes = vec![
                        KeyValue::new(
                            "agent.context.block.position",
                            as_i64(block.position as u64),
                        ),
                        KeyValue::new(
                            "agent.context.block.allocated_tokens",
                            as_i64(block.allocated_tokens as u64),
                        ),
                        KeyValue::new(
                            "agent.context.block.final_tokens",
                            as_i64(block.final_tokens as u64),
                        ),
                        KeyValue::new("agent.context.block.outcome", outcome),
                    ];
                    if let Some(tokens) = block.original_tokens {
                        attributes.push(KeyValue::new(
                            "agent.context.block.original_tokens",
                            as_i64(tokens as u64),
                        ));
                    }
                    if let Some(reason) = reason {
                        attributes
                            .push(KeyValue::new("agent.context.block.reason", reason.clone()));
                    }
                    self.identifier(&mut attributes, "agent.context.block.id", &block.id);
                    if let Some(version) = &block.source_version {
                        self.identifier(
                            &mut attributes,
                            "agent.context.block.source_version",
                            version,
                        );
                    }
                    span.add_event("agent.context.block", attributes);
                }
            }
            AgentEvent::ContextAssemblyFailed(report) => {
                let mut attributes = vec![
                    KeyValue::new("error.type", report.error.kind.clone()),
                    KeyValue::new("agent.model.step", as_i64(report.step as u64)),
                ];
                if let Some(block) = report.blocks.last() {
                    attributes.push(KeyValue::new(
                        "agent.context.block.position",
                        as_i64(block.position as u64),
                    ));
                    self.identifier(&mut attributes, "agent.context.block.id", &block.id);
                }
                // The run/node error event also reports the original typed cause
                // through the configured redactor. This event contains metadata only.
                parent.span().add_event("agent.context.failed", attributes);
            }
            AgentEvent::ModelDispatched(report) => {
                // The model middleware's span is current at the terminal dispatch
                // point, including auxiliary calls. No observer queue is involved.
                let current = Context::current();
                let span = current.span();
                span.set_attributes(vec![
                    KeyValue::new(
                        "agent.model.sent.estimated_input_tokens",
                        as_i64(report.estimated_input_tokens),
                    ),
                    KeyValue::new(
                        "agent.model.sent.message_count",
                        as_i64(report.message_count as u64),
                    ),
                    KeyValue::new(
                        "agent.model.sent.tool_count",
                        as_i64(report.tool_count as u64),
                    ),
                    KeyValue::new(
                        "agent.model.sent.output_token_limit",
                        i64::from(report.output_token_limit),
                    ),
                    KeyValue::new(
                        "agent.model.sent.input_capacity",
                        as_i64(report.input_capacity as u64),
                    ),
                ]);
                self.metrics.context_tokens.record(
                    report.estimated_input_tokens,
                    &[KeyValue::new("agent.context.phase", "dispatched")],
                );
            }
            _ => {}
        }
    }
    async fn on_event(&self, _: &RunContext, _: &AgentEvent) {}
}
