use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_otel::*;
use eliteforge_agent_test_support::*;
use opentelemetry::{
    Context,
    trace::{Status, TraceContextExt, Tracer, TracerProvider},
};
use opentelemetry_sdk::{
    metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider},
    trace::{InMemorySpanExporter, SdkTracerProvider},
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::sync::Notify;

struct FixtureModel;
#[async_trait]
impl ChatModel for FixtureModel {
    async fn generate(
        &self,
        _model_id: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        let mut response = if request.messages.iter().any(|m| m.role == Role::Tool) {
            ChatResponse::text("private-model-response")
        } else {
            ChatResponse::tool_calls(vec![ToolCall::new(
                "call-1",
                "private_tool",
                json!({"secret":"private-arguments"}),
            )])
        };
        response.usage = Usage {
            input_tokens: 17,
            output_tokens: 3,
        };
        Ok(
            response.with_provider_metrics(eliteforge_agent_core::ProviderMetrics {
                cached_input_tokens: Some(5),
                reasoning_tokens: None,
            }),
        )
    }
}
struct FixtureTool;
#[async_trait]
impl Tool for FixtureTool {
    async fn call(&self, _: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::error("private-tool-error"))
    }
}

#[tokio::test]
async fn real_exporters_record_parentage_tool_link_usage_and_no_payloads() {
    let exporter = InMemorySpanExporter::default();
    let traces = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let metric_exporter = InMemoryMetricExporter::default();
    let metrics = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(metric_exporter.clone()).build())
        .build();
    let parent = Context::new().with_span(traces.tracer("host").start("host-operation"));
    let parent_id = parent.span().span_context().span_id();
    let agent = TestBuilder::default()
        .plugin(
            TelemetryPlugin::new("telemetry", &traces, &metrics, TelemetryConfig::default())
                .unwrap(),
        )
        .plugin(ChatModelPlugin::new(FixtureModel))
        .plugin(ToolProviderPlugin::new(
            "tools",
            StaticToolProvider::new(vec![ToolBinding::new(
                ToolDescriptor::new(
                    "private-id",
                    "private_tool",
                    "private-description",
                    json!({"type":"object"}),
                ),
                FixtureTool,
            )]),
        ))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("private-session", "private-prompt");
    request.extensions.insert(TraceParent(parent.clone()));
    agent.run(request).await.unwrap();
    assert!(agent.framework().close().await.is_success());
    // Closing the Agent does not shut down host-owned providers.
    parent.span().end();
    let mut after = traces.tracer("host").start("after-agent-close");
    opentelemetry::trace::Span::end(&mut after);
    traces.force_flush().unwrap();
    metrics.force_flush().unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    let run = spans.iter().find(|s| s.name == "agent.run").unwrap();
    assert_eq!(run.parent_span_id, parent_id);
    let models: Vec<_> = spans.iter().filter(|s| s.name == "agent.model").collect();
    assert_eq!(models.len(), 2);
    assert!(
        models
            .iter()
            .all(|s| s.parent_span_id == run.span_context.span_id())
    );
    let tool = spans.iter().find(|s| s.name == "agent.tool").unwrap();
    assert_eq!(tool.parent_span_id, run.span_context.span_id());
    assert_eq!(tool.links.links.len(), 1);
    assert_eq!(
        tool.links.links[0].span_context.span_id(),
        models[0].span_context.span_id()
    );
    assert!(matches!(tool.status, Status::Error { .. }));
    assert!(spans.iter().any(|s| s.name == "after-agent-close"));
    assert!(models.iter().all(|s| {
        s.attributes
            .iter()
            .any(|a| a.key.as_str() == "gen_ai.usage.input_tokens" && a.value.to_string() == "17")
    }));
    assert!(models.iter().all(|s| {
        s.attributes.iter().any(|a| {
            a.key.as_str() == "gen_ai.usage.cached_input_tokens" && a.value.to_string() == "5"
        })
    }));
    let metric_data = metric_exporter.get_finished_metrics().unwrap();
    let observed = format!("{spans:?} {metric_data:?}");
    for private in [
        "private-prompt",
        "private-session",
        "private_tool",
        "private-id",
        "private-arguments",
        "private-model-response",
        "private-tool-error",
    ] {
        assert!(!observed.contains(private), "export leaked {private}");
    }
    assert!(observed.contains("agent.operation.duration"));
    assert!(observed.contains("agent.model.token.usage"));
    assert!(observed.contains("agent.operation.errors"));
}

struct PendingModel(Arc<Notify>);
#[async_trait]
impl ChatModel for PendingModel {
    async fn generate(
        &self,
        _model_id: &str,
        _: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        self.0.notify_one();
        std::future::pending().await
    }
}
#[tokio::test]
async fn cancelled_model_future_ends_spans_with_cancelled_outcome() {
    let exporter = InMemorySpanExporter::default();
    let traces = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let metrics = SdkMeterProvider::default();
    let entered = Arc::new(Notify::new());
    let agent = TestBuilder::default()
        .plugin(
            TelemetryPlugin::new("telemetry", &traces, &metrics, TelemetryConfig::default())
                .unwrap(),
        )
        .plugin(ChatModelPlugin::new(PendingModel(entered.clone())))
        .build()
        .await
        .unwrap();
    let handle = agent.start(RunRequest::new("s", "prompt")).unwrap();
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    agent.cancel(&handle).unwrap();
    assert!(matches!(
        agent.wait(handle).await,
        Err(AgentError::Cancelled)
    ));
    agent.framework().close().await;
    let spans = exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 2);
    assert!(
        spans.iter().all(|s| s
            .attributes
            .iter()
            .any(|a| a.key.as_str() == "agent.operation.outcome"
                && a.value.to_string() == "cancelled"))
    );
    assert!(spans.iter().all(|s| s.status != Status::Ok));
}

struct FailedToolShutdown;
#[async_trait]
impl ToolProvider for FailedToolShutdown {
    async fn lease(&self, _: &RunContext) -> Result<ToolLease, AgentError> {
        Ok(ToolLease::new(vec![]).with_shutdown(|| async { Err("private-shutdown-error".into()) }))
    }
}
#[tokio::test]
async fn cleanup_failure_does_not_reclassify_a_cancelled_run() {
    let exporter = InMemorySpanExporter::default();
    let traces = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let metrics = SdkMeterProvider::default();
    let entered = Arc::new(Notify::new());
    let agent = TestBuilder::default()
        .plugin(
            TelemetryPlugin::new("telemetry", &traces, &metrics, TelemetryConfig::default())
                .unwrap(),
        )
        .plugin(ChatModelPlugin::new(PendingModel(entered.clone())))
        .plugin(ToolProviderPlugin::new(
            "failed-shutdown",
            FailedToolShutdown,
        ))
        .build()
        .await
        .unwrap();
    let handle = agent.start(RunRequest::new("s", "prompt")).unwrap();
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    agent.cancel(&handle).unwrap();
    let error = agent.wait(handle).await.unwrap_err();
    assert!(matches!(error, AgentError::WithCleanup { .. }));
    assert!(matches!(error.primary(), AgentError::Cancelled));
    agent.framework().close().await;
    let spans = exporter.get_finished_spans().unwrap();
    let run = spans.iter().find(|s| s.name == "agent.run").unwrap();
    assert!(
        run.attributes
            .iter()
            .any(|a| a.key.as_str() == "agent.operation.outcome"
                && a.value.to_string() == "cancelled")
    );
    assert!(!format!("{spans:?}").contains("private-shutdown-error"));
}

use eliteforge_agent_official_plugin_storage_local::{SqliteOptions, SqliteStateStore};
use opentelemetry_sdk::trace::SpanData;
use std::sync::Mutex;

fn attr(span: &SpanData, key: &str) -> String {
    span.attributes
        .iter()
        .find(|a| a.key.as_str() == key)
        .unwrap_or_else(|| panic!("missing {key} in {}", span.name))
        .value
        .to_string()
}

async fn database(path: &std::path::Path) -> Arc<SqliteStateStore> {
    Arc::new(
        SqliteStateStore::open(path, SqliteOptions::default())
            .await
            .unwrap(),
    )
}
struct ToolBarrier(tokio::sync::Barrier);
#[async_trait]
impl ToolMiddleware for ToolBarrier {
    async fn intercept(
        &self,
        _: &ToolInvocationContext<'_>,
        next: ToolNext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        self.0.wait().await;
        next.run().await
    }
}

struct ResumeNode(Option<Arc<Notify>>);

struct ErrorNode;
struct SafeErrors;
impl ErrorRedactor for SafeErrors {
    fn redact(&self, kind: &str, _: &str) -> Option<String> {
        Some(format!("sanitized {kind}"))
    }
}
struct OrderMiddleware(&'static str, Arc<Mutex<Vec<String>>>);

struct BadNode;
struct Deny;

#[test]
fn error_reports_bound_text_and_causes_and_fail_closed_on_redactor_panic() {
    struct Long;
    impl ErrorRedactor for Long {
        fn redact(&self, _: &str, _: &str) -> Option<String> {
            Some("界".repeat(2000))
        }
    }
    struct Panics;
    impl ErrorRedactor for Panics {
        fn redact(&self, _: &str, _: &str) -> Option<String> {
            panic!("redactor failure")
        }
    }
    let mut error = AgentError::Run("secret".into());
    for _ in 0..30 {
        error = AgentError::WithCleanup {
            primary: Box::new(error),
            cleanup: Box::new(AgentError::Run("cleanup".into())),
        };
    }
    fn count(report: &ErrorReport) -> usize {
        1 + report
            .causes
            .iter()
            .chain(&report.cleanup)
            .map(count)
            .sum::<usize>()
    }
    let report = error.report(&Long);
    assert!(report.truncated);
    assert!(report.message.as_ref().unwrap().len() <= 2048);
    assert!(count(&report) <= 16);
    assert!(error.report(&Panics).message.is_none());
    assert!(
        !serde_json::to_string(&error.report(&OmitErrorMessages))
            .unwrap()
            .contains("secret")
    );
}

struct PendingTool(Arc<Notify>);
#[async_trait]
impl Tool for PendingTool {
    async fn call(&self, _: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        self.0.notify_one();
        std::future::pending().await
    }
}
struct FailAfterTool(Arc<Notify>);

struct RequireApproval;
#[async_trait]
impl ToolPolicy for RequireApproval {
    async fn evaluate(
        &self,
        _: &ToolInvocationContext<'_>,
    ) -> Result<ToolPolicyDecision, AgentError> {
        Ok(ToolPolicyDecision::RequireApproval("Approve?".into()))
    }
}

struct FailedModel;
#[async_trait]
impl ChatModel for FailedModel {
    async fn generate(
        &self,
        _model_id: &str,
        _: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        Err(ModelError::Fatal("private-provider-body".into()))
    }
}

struct CaptureToolContext(Arc<Mutex<Option<opentelemetry::trace::SpanId>>>);
#[async_trait]
impl Tool for CaptureToolContext {
    async fn call(&self, _: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        *self.0.lock().unwrap() = Some(Context::current().span().span_context().span_id());
        Ok(ToolOutput::text("ok"))
    }
}
#[tokio::test]
async fn tool_interceptor_attaches_its_span_while_polling_the_executor() {
    let exporter = InMemorySpanExporter::default();
    let traces = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let metrics = SdkMeterProvider::default();
    let observed = Arc::new(Mutex::new(None));
    let a = TestBuilder::default()
        .plugin(
            TelemetryPlugin::new("telemetry", &traces, &metrics, TelemetryConfig::default())
                .unwrap(),
        )
        .plugin(ChatModelPlugin::new(FixtureModel))
        .plugin(ToolProviderPlugin::new(
            "tools",
            StaticToolProvider::new(vec![ToolBinding::new(
                ToolDescriptor::new("tool", "private_tool", "", json!({})),
                CaptureToolContext(observed.clone()),
            )]),
        ))
        .build()
        .await
        .unwrap();
    a.run(RunRequest::new("s", "input")).await.unwrap();
    a.framework().close().await;
    let spans = exporter.get_finished_spans().unwrap();
    let tool = spans.iter().find(|s| s.name == "agent.tool").unwrap();
    assert_eq!(*observed.lock().unwrap(), Some(tool.span_context.span_id()));
}

#[derive(Clone, Default)]
struct FinalRequestCapture(Arc<std::sync::Mutex<Vec<ChatRequest>>>);
#[async_trait]
impl ChatModel for FinalRequestCapture {
    async fn generate(
        &self,
        _model_id: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        self.0.lock().unwrap().push(request);
        Ok(ChatResponse::text("private-response"))
    }
}
struct AddPrivateInput;
#[async_trait]
impl ModelMiddleware for AddPrivateInput {
    async fn intercept(
        &self,
        mut request: ChatRequest,
        context: &ModelInvocationContext<'_>,
        next: ModelNext<'_>,
    ) -> Result<ChatResponse, ModelError> {
        request
            .messages
            .push(Message::user("private-middleware-input"));
        next.run(request, context).await
    }
}
#[tokio::test]
async fn telemetry_distinguishes_assembly_from_final_dispatch_after_middleware_without_payloads() {
    let exporter = InMemorySpanExporter::default();
    let traces = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let metrics = SdkMeterProvider::builder().build();
    let capture = FinalRequestCapture::default();
    let agent = TestBuilder::default()
        .plugin(
            TelemetryPlugin::new("telemetry", &traces, &metrics, TelemetryConfig::default())
                .unwrap(),
        )
        .plugin(ModelMiddlewarePlugin::new("rewrite", AddPrivateInput))
        .plugin(ChatModelPlugin::new(capture.clone()))
        .plugin(ContextContributorPlugin::new(
            "instructions",
            FixedInstructions::new("fixed.instructions", "private-instructions"),
        ))
        .build()
        .await
        .unwrap();
    agent
        .run(RunRequest::new("private-session", "private-input"))
        .await
        .unwrap();
    agent.framework().close().await;
    traces.force_flush().unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    let run = spans.iter().find(|span| span.name == "agent.run").unwrap();
    let model = spans
        .iter()
        .find(|span| span.name == "agent.model")
        .unwrap();
    let request = capture.0.lock().unwrap()[0].clone();
    let estimate = ConservativeTokenEstimator
        .request(&ModelRef::new("default", "default"), &request)
        .await
        .unwrap();
    let attribute = |name: &str| {
        model
            .attributes
            .iter()
            .find(|a| a.key.as_str() == name)
            .unwrap()
            .value
            .to_string()
    };
    assert_eq!(attribute("agent.model.sent.message_count"), "3");
    assert_eq!(
        attribute("agent.model.sent.estimated_input_tokens"),
        estimate.to_string()
    );
    let assembly = run
        .events
        .iter()
        .find(|event| event.name == "agent.context.assembled")
        .unwrap();
    let assembled: usize = assembly
        .attributes
        .iter()
        .find(|a| a.key.as_str() == "agent.context.estimated_input_tokens")
        .unwrap()
        .value
        .to_string()
        .parse()
        .unwrap();
    assert!(assembled < estimate);
    assert_eq!(model.parent_span_id, run.span_context.span_id());
    assert!(!format!("{spans:?}").contains("private-"));
    traces.shutdown().unwrap();
    metrics.shutdown().unwrap();
}

struct FailingContextSource;
#[async_trait]
impl ContextContributor for FailingContextSource {
    fn block_id(&self) -> &str {
        "private-block"
    }
    async fn contribute(&self, _: &ContextInput<'_>) -> Result<ContextBlock, AgentError> {
        Err(AgentError::Run("private-source-error".into()))
    }
}
#[tokio::test]
async fn context_failure_keeps_block_metadata_and_typed_cause_but_exports_no_error_body() {
    let exporter = InMemorySpanExporter::default();
    let traces = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let metrics = SdkMeterProvider::builder().build();
    let capture = FinalRequestCapture::default();
    let agent = TestBuilder::default()
        .plugin(
            TelemetryPlugin::new("telemetry", &traces, &metrics, TelemetryConfig::default())
                .unwrap(),
        )
        .plugin(ChatModelPlugin::new(capture.clone()))
        .plugin(ContextContributorPlugin::new(
            "source",
            FailingContextSource,
        ))
        .plugin(ContextLayoutPlugin(ContextLayout(vec![
            ContextBlockConfig::new(INSTRUCTIONS_BLOCK, BlockBudget::tokens(0)),
            ContextBlockConfig::new("private-block", BlockBudget::tokens(100)),
            ContextBlockConfig::new(HISTORY_BLOCK, BlockBudget::tokens(1000)),
        ])))
        .build()
        .await
        .unwrap();
    let error = agent
        .run(RunRequest::new("private-session", "private-input"))
        .await
        .unwrap_err();
    assert!(
        matches!(error.primary(), AgentError::Run(message) if message == "private-source-error")
    );
    let AgentError::ContextAssembly { report, .. } = &error else {
        panic!("missing assembly report")
    };
    assert_eq!(report.blocks.last().unwrap().id, "private-block");
    assert_eq!(report.blocks.last().unwrap().position, 1);
    assert!(report.error.message.is_none());
    assert!(capture.0.lock().unwrap().is_empty());
    agent.framework().close().await;
    traces.force_flush().unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    let run = spans.iter().find(|span| span.name == "agent.run").unwrap();
    assert!(
        run.events
            .iter()
            .any(|event| event.name == "agent.context.failed")
    );
    assert!(run.events.iter().any(|event| event.name == "agent.error"));
    assert!(!format!("{spans:?}").contains("private-"));
    traces.shutdown().unwrap();
    metrics.shutdown().unwrap();
}

struct SummaryCall;
#[async_trait]
impl AgentLoop for SummaryCall {
    async fn run(
        &self,
        request: &RunRequest,
        context: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let output = services
            .models
            .as_ref()
            .unwrap()
            .invoke_auxiliary(
                &ModelRef::new("default", "default"),
                ChatRequest {
                    messages: vec![Message::user("private-history")],
                    tools: vec![],
                    extensions: context.extensions.clone(),
                    options: GenerationOptions::default(),
                },
                "context_compression",
                &request.limits,
                context,
            )
            .await?;
        Ok(RunResult {
            output: output.content.text(),
            content: output.content,
            structured: output.structured,
            interactions: vec![],
            status: RunStatus::Completed,
            usage: context.usage(),
            steps: 0,
            tool_calls: 0,
            external_session: None,
        })
    }
}
#[tokio::test]
async fn auxiliary_summary_dispatch_has_its_own_model_span_and_purpose() {
    let exporter = InMemorySpanExporter::default();
    let traces = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let metrics = SdkMeterProvider::default();
    let agent = TestBuilder::default()
        .plugin(
            TelemetryPlugin::new("telemetry", &traces, &metrics, TelemetryConfig::default())
                .unwrap(),
        )
        .plugin(ChatModelPlugin::new(FinalRequestCapture::default()))
        .plugin(AgentLoopPlugin::new(SummaryCall))
        .build()
        .await
        .unwrap();
    agent.run(RunRequest::new("s", "unused")).await.unwrap();
    agent.framework().close().await;
    traces.force_flush().unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    let model = spans
        .iter()
        .find(|span| span.name == "agent.model")
        .unwrap();
    let run = spans.iter().find(|span| span.name == "agent.run").unwrap();
    assert_eq!(attr(model, "agent.model.summary"), "true");
    assert_eq!(attr(model, "agent.model.sent.message_count"), "1");
    assert_eq!(model.parent_span_id, run.span_context.span_id());
    assert!(!format!("{spans:?}").contains("private-history"));
    traces.shutdown().unwrap();
    metrics.shutdown().unwrap();
}

#[tokio::test]
async fn context_no_gain_exports_explicit_outcome_without_source_payload() {
    let exporter = InMemorySpanExporter::default();
    let traces = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let metrics = SdkMeterProvider::builder().build();
    let capture = FinalRequestCapture::default();
    let body = format!("private-no-gain-source{}", "X".repeat(100));
    let agent = TestBuilder::default()
        .plugin(
            TelemetryPlugin::new("telemetry", &traces, &metrics, TelemetryConfig::default())
                .unwrap(),
        )
        .plugin(ChatModelPlugin::new(capture.clone()))
        .plugin(ContextContributorPlugin::new(
            "source",
            FixedContextContributor(ContextBlock::data("private-block", body.clone())),
        ))
        .plugin(ContextLayoutPlugin(ContextLayout(vec![
            ContextBlockConfig::new(INSTRUCTIONS_BLOCK, BlockBudget::tokens(0)),
            ContextBlockConfig::new("private-block", BlockBudget::tokens(500))
                .compress_at(1000, 500),
            ContextBlockConfig::new(HISTORY_BLOCK, BlockBudget::tokens(1000)),
        ])))
        .build()
        .await
        .unwrap();
    agent
        .run(RunRequest::new("private-session", "private-input"))
        .await
        .unwrap();
    assert!(agent.framework().close().await.is_success());
    traces.force_flush().unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    let run = spans.iter().find(|span| span.name == "agent.run").unwrap();
    let event = run
        .events
        .iter()
        .find(|event| {
            event.name == "agent.context.block"
                && event.attributes.iter().any(|attribute| {
                    attribute.key.as_str() == "agent.context.block.position"
                        && attribute.value.to_string() == "1"
                })
        })
        .unwrap();
    assert!(
        event
            .attributes
            .iter()
            .any(
                |attribute| attribute.key.as_str() == "agent.context.block.outcome"
                    && attribute.value.to_string() == "no_gain"
            )
    );
    assert!(
        capture.0.lock().unwrap()[0]
            .messages
            .iter()
            .any(|message| message.content.text() == body)
    );
    assert!(!format!("{spans:?}").contains("private-"));
    traces.shutdown().unwrap();
    metrics.shutdown().unwrap();
}
