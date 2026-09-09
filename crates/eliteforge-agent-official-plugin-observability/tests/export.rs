use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_observability::*;
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
        Ok(response)
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
    assert!(agent.close().await.is_success());
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
    handle.cancel();
    assert!(matches!(handle.wait().await, Err(AgentError::Cancelled)));
    agent.close().await;
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
    handle.cancel();
    let error = handle.wait().await.unwrap_err();
    assert!(matches!(error, AgentError::WithCleanup { .. }));
    assert!(matches!(error.primary(), AgentError::Cancelled));
    agent.close().await;
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

use eliteforge_agent_state_sqlite::{SqliteOptions, SqliteStateStore};
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
fn graph(nodes: Vec<DagNode>) -> DagWorkflowDefinition {
    DagWorkflowDefinition {
        id: "private-workflow".into(),
        version: 1,
        max_parallel_nodes: 2,
        nodes,
        edges: vec![],
    }
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

#[tokio::test]
async fn parallel_dag_spans_and_duplicate_provider_call_ids_stay_with_their_node() {
    let temp = tempfile::tempdir().unwrap();
    let db = database(&temp.path().join("parallel.db")).await;
    let exporter = InMemorySpanExporter::default();
    let traces = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let metrics = SdkMeterProvider::default();
    let agent = TestBuilder::default()
        // Both model responses must have been observed before either tool span starts.
        .plugin(ToolMiddlewarePlugin::new(
            "barrier",
            ToolBarrier(tokio::sync::Barrier::new(2)),
        ))
        .plugin(
            TelemetryPlugin::new("telemetry", &traces, &metrics, TelemetryConfig::default())
                .unwrap(),
        )
        .plugin(StateStorePlugin(db.clone()))
        .plugin(AgentLoopPlugin::new(
            DagWorkflowLoop::new(graph(vec![
                DagNode::react(WorkflowStage::new("left")),
                DagNode::react(WorkflowStage::new("right")),
            ]))
            .unwrap(),
        ))
        .plugin(ChatModelPlugin::new(FixtureModel))
        .plugin(ToolProviderPlugin::new(
            "tools",
            StaticToolProvider::new(vec![
                ToolBinding::new(
                    ToolDescriptor::new(
                        "private-id",
                        "private_tool",
                        "private-description",
                        json!({"type":"object"}),
                    ),
                    FixtureTool,
                )
                .parallel_safe(),
            ]),
        ))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("private-session", "private-prompt");
    request.limits.max_parallel_tools = 2;
    tokio::time::timeout(Duration::from_secs(5), agent.run(request))
        .await
        .unwrap()
        .unwrap();
    agent.close().await;
    db.close().await.unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    let run = spans.iter().find(|s| s.name == "agent.run").unwrap();
    let nodes: Vec<_> = spans
        .iter()
        .filter(|s| s.name == "agent.workflow.node")
        .collect();
    assert_eq!(nodes.len(), 2);
    assert_ne!(
        attr(nodes[0], "agent.node.attempt.id"),
        attr(nodes[1], "agent.node.attempt.id")
    );
    for node in nodes {
        assert_eq!(node.parent_span_id, run.span_context.span_id());
        assert_eq!(
            attr(node, "agent.execution.id"),
            attr(run, "agent.execution.id")
        );
        let models: Vec<_> = spans
            .iter()
            .filter(|s| s.name == "agent.model" && s.parent_span_id == node.span_context.span_id())
            .collect();
        assert_eq!(models.len(), 2);
        let tool = spans
            .iter()
            .find(|s| s.name == "agent.tool" && s.parent_span_id == node.span_context.span_id())
            .unwrap();
        assert_eq!(tool.links.links.len(), 1);
        assert!(models.iter().any(
            |model| model.span_context.span_id() == tool.links.links[0].span_context.span_id()
        ));
        assert_eq!(
            attr(tool, "agent.node.invocation.id"),
            attr(node, "agent.node.invocation.id")
        );
        assert_eq!(tool.events.events[0].name, "agent.error");
    }
    let dump = format!("{spans:?}");
    for secret in [
        "private-workflow",
        "private-session",
        "private-prompt",
        "private-tool-error",
    ] {
        assert!(!dump.contains(secret));
    }
}

struct ResumeNode(Option<Arc<Notify>>);
#[async_trait]
impl WorkflowNode for ResumeNode {
    fn kind(&self) -> &str {
        "resume"
    }
    fn supports_resume(&self) -> bool {
        true
    }
    async fn execute(
        &self,
        input: WorkflowNodeInput,
        context: &WorkflowNodeContext,
        _: &AgentServices,
    ) -> Result<WorkflowNodeResult, AgentError> {
        if input.checkpoint.is_none() {
            context.save(json!({"saved":true})).await?;
            if let Some(entered) = &self.0 {
                entered.notify_one();
                std::future::pending::<()>().await;
            }
            return Err(AgentError::Run("private-error".into()));
        }
        Ok(WorkflowNodeResult::completed(json!("ok")))
    }
}

#[tokio::test]
async fn explicit_retry_after_reopening_links_attempts_and_allocates_a_new_invocation() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("resume.db");
    let exporter = InMemorySpanExporter::default();
    let traces = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let metrics = SdkMeterProvider::default();
    let definition = graph(vec![DagNode::new("private-node", "resume", json!({}))]);
    let db = database(&path).await;
    let a = TestBuilder::default()
        .plugin(
            TelemetryPlugin::new("telemetry", &traces, &metrics, TelemetryConfig::default())
                .unwrap(),
        )
        .plugin(StateStorePlugin(db.clone()))
        .plugin(WorkflowNodePlugin::new("nodes", ResumeNode(None)))
        .plugin(AgentLoopPlugin::new(
            DagWorkflowLoop::new(definition.clone()).unwrap(),
        ))
        .build()
        .await
        .unwrap();
    let h = a.start(RunRequest::new("session", "input")).unwrap();
    let run_id = h.run_id;
    assert!(h.wait().await.is_err());
    let before: DagWorkflowState =
        serde_json::from_value(db.load(run_id).await.unwrap().state).unwrap();
    let invocation = before.nodes["private-node"].invocation_id;
    a.close().await;
    db.close().await.unwrap();
    drop(a);
    drop(db);
    let db = database(&path).await;
    let a = TestBuilder::default()
        .plugin(
            TelemetryPlugin::new("telemetry", &traces, &metrics, TelemetryConfig::default())
                .unwrap(),
        )
        .plugin(StateStorePlugin(db.clone()))
        .plugin(WorkflowNodePlugin::new("nodes", ResumeNode(None)))
        .plugin(AgentLoopPlugin::new(
            DagWorkflowLoop::new(definition).unwrap(),
        ))
        .build()
        .await
        .unwrap();
    // Explicit retry creates a new invocation, retaining the predecessor attempt.
    let mut request = RunRequest::new("session", "");
    request.resume = Some(ResumeRequest {
        run_id,
        resolutions: vec![CallResolution::RetryNode {
            node_id: "private-node".into(),
            invocation_id: invocation,
        }],
        ..Default::default()
    });
    // Retry clears local state, so this fixture intentionally fails again.
    assert!(a.run(request).await.is_err());
    a.close().await;
    let after: DagWorkflowState =
        serde_json::from_value(db.load(run_id).await.unwrap().state).unwrap();
    db.close().await.unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    let nodes: Vec<_> = spans
        .iter()
        .filter(|s| s.name == "agent.workflow.node")
        .collect();
    assert_eq!(nodes.len(), 2);
    assert_eq!(
        attr(nodes[0], "agent.run.id"),
        attr(nodes[1], "agent.run.id")
    );
    assert_ne!(
        attr(nodes[0], "agent.execution.id"),
        attr(nodes[1], "agent.execution.id")
    );
    assert_ne!(
        attr(nodes[0], "agent.node.invocation.id"),
        attr(nodes[1], "agent.node.invocation.id")
    );
    assert_eq!(
        attr(nodes[1], "agent.node.previous_attempt.id"),
        attr(nodes[0], "agent.node.attempt.id")
    );
    assert_eq!(
        attr(nodes[1], "agent.previous_execution.id"),
        attr(nodes[0], "agent.execution.id")
    );
    assert_eq!(
        after.nodes["private-node"]
            .last_execution
            .as_ref()
            .unwrap()
            .attempt_id
            .to_string(),
        attr(nodes[1], "agent.node.attempt.id")
    );
    assert!(!format!("{spans:?}").contains("private-error"));
}

struct ErrorNode;
#[async_trait]
impl WorkflowNode for ErrorNode {
    fn kind(&self) -> &str {
        "error"
    }
    async fn execute(
        &self,
        _: WorkflowNodeInput,
        _: &WorkflowNodeContext,
        _: &AgentServices,
    ) -> Result<WorkflowNodeResult, AgentError> {
        Err(AgentError::WithCleanup {
            primary: Box::new(AgentError::Run("token=secret-value".into())),
            cleanup: Box::new(AgentError::Run("cleanup-secret".into())),
        })
    }
}
struct SafeErrors;
impl ErrorRedactor for SafeErrors {
    fn redact(&self, kind: &str, _: &str) -> Option<String> {
        Some(format!("sanitized {kind}"))
    }
}
struct OrderMiddleware(&'static str, Arc<Mutex<Vec<String>>>);
#[async_trait]
impl WorkflowNodeMiddleware for OrderMiddleware {
    async fn intercept(
        &self,
        input: WorkflowNodeInput,
        context: &WorkflowNodeContext,
        next: WorkflowNodeNext<'_>,
    ) -> Result<WorkflowNodeResult, AgentError> {
        self.1.lock().unwrap().push(format!("{} before", self.0));
        let result = next.run(input, context).await;
        self.1.lock().unwrap().push(format!("{} after", self.0));
        result
    }
}
#[tokio::test]
async fn node_middleware_unwinds_in_order_and_exports_redacted_primary_and_cleanup_errors() {
    let temp = tempfile::tempdir().unwrap();
    let db = database(&temp.path().join("errors.db")).await;
    let exporter = InMemorySpanExporter::default();
    let traces = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let metrics = SdkMeterProvider::default();
    let order = Arc::new(Mutex::new(vec![]));
    let a = TestBuilder::default()
        .plugin(
            TelemetryPlugin::new("telemetry", &traces, &metrics, TelemetryConfig::default())
                .unwrap()
                .with_error_redactor(Arc::new(SafeErrors)),
        )
        .plugin(WorkflowNodeMiddlewarePlugin::new(
            "outer",
            OrderMiddleware("outer", order.clone()),
        ))
        .plugin(WorkflowNodeMiddlewarePlugin::new(
            "inner",
            OrderMiddleware("inner", order.clone()),
        ))
        .plugin(StateStorePlugin(db.clone()))
        .plugin(WorkflowNodePlugin::new("nodes", ErrorNode))
        .plugin(AgentLoopPlugin::new(
            DagWorkflowLoop::new(graph(vec![DagNode::new("error", "error", json!({}))])).unwrap(),
        ))
        .build()
        .await
        .unwrap();
    assert!(a.run(RunRequest::new("s", "input")).await.is_err());
    a.close().await;
    db.close().await.unwrap();
    assert_eq!(
        *order.lock().unwrap(),
        ["outer before", "inner before", "inner after", "outer after"]
    );
    let spans = exporter.get_finished_spans().unwrap();
    for span in spans
        .iter()
        .filter(|s| s.name == "agent.run" || s.name == "agent.workflow.node")
    {
        assert!(matches!(span.status, Status::Error { .. }));
        let report = span
            .events
            .events
            .iter()
            .find(|e| e.name == "agent.error")
            .unwrap()
            .attributes
            .iter()
            .find(|a| a.key.as_str() == "agent.error.report")
            .unwrap()
            .value
            .to_string();
        let report: ErrorReport = serde_json::from_str(&report).unwrap();
        assert_eq!(report.causes.len(), 1);
        assert_eq!(report.cleanup.len(), 1);
        assert_eq!(report.cleanup[0].message.as_deref(), Some("sanitized run"));
    }
    let dump = format!("{spans:?}");
    assert!(!dump.contains("secret-value"));
    assert!(!dump.contains("cleanup-secret"));
}

#[tokio::test]
async fn cancelled_dag_node_ends_its_model_and_node_spans() {
    let temp = tempfile::tempdir().unwrap();
    let db = database(&temp.path().join("cancel.db")).await;
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
        .plugin(StateStorePlugin(db.clone()))
        .plugin(ChatModelPlugin::new(PendingModel(entered.clone())))
        .plugin(AgentLoopPlugin::new(
            DagWorkflowLoop::new(graph(vec![DagNode::react(WorkflowStage::new("pending"))]))
                .unwrap(),
        ))
        .build()
        .await
        .unwrap();
    let handle = agent.start(RunRequest::new("s", "input")).unwrap();
    tokio::time::timeout(Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    handle.cancel();
    assert!(matches!(handle.wait().await, Err(AgentError::Cancelled)));
    agent.close().await;
    db.close().await.unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 3);
    assert!(
        spans
            .iter()
            .all(|s| attr(s, "agent.operation.outcome") == "cancelled" && s.status != Status::Ok)
    );
}

#[tokio::test]
async fn interrupted_node_continuation_keeps_invocation_across_agent_recreation() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("continue.db");
    let exporter = InMemorySpanExporter::default();
    let traces = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let metrics = SdkMeterProvider::default();
    let definition = graph(vec![DagNode::new("node", "resume", json!({}))]);
    let entered = Arc::new(Notify::new());
    let db = database(&path).await;
    let a = TestBuilder::default()
        .plugin(
            TelemetryPlugin::new("telemetry", &traces, &metrics, TelemetryConfig::default())
                .unwrap(),
        )
        .plugin(StateStorePlugin(db.clone()))
        .plugin(WorkflowNodePlugin::new(
            "nodes",
            ResumeNode(Some(entered.clone())),
        ))
        .plugin(AgentLoopPlugin::new(
            DagWorkflowLoop::new(definition.clone()).unwrap(),
        ))
        .build()
        .await
        .unwrap();
    let h = a.start(RunRequest::new("s", "input")).unwrap();
    let run_id = h.run_id;
    tokio::time::timeout(Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    h.cancel();
    assert!(h.wait().await.is_err());
    a.close().await;
    db.close().await.unwrap();
    drop(a);
    drop(db);
    let db = database(&path).await;
    let a = TestBuilder::default()
        .plugin(
            TelemetryPlugin::new("telemetry", &traces, &metrics, TelemetryConfig::default())
                .unwrap(),
        )
        .plugin(StateStorePlugin(db.clone()))
        .plugin(WorkflowNodePlugin::new("nodes", ResumeNode(None)))
        .plugin(AgentLoopPlugin::new(
            DagWorkflowLoop::new(definition).unwrap(),
        ))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("s", "");
    request.resume = Some(ResumeRequest {
        run_id,
        ..Default::default()
    });
    assert_eq!(a.run(request).await.unwrap().status, RunStatus::Completed);
    a.close().await;
    db.close().await.unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    let nodes: Vec<_> = spans
        .iter()
        .filter(|s| s.name == "agent.workflow.node")
        .collect();
    assert_eq!(nodes.len(), 2);
    assert_eq!(
        attr(nodes[0], "agent.node.invocation.id"),
        attr(nodes[1], "agent.node.invocation.id")
    );
    assert_ne!(
        attr(nodes[0], "agent.node.attempt.id"),
        attr(nodes[1], "agent.node.attempt.id")
    );
    assert_eq!(
        attr(nodes[0], "agent.node.attempt.id"),
        attr(nodes[1], "agent.node.previous_attempt.id")
    );
    assert_eq!(
        attr(nodes[0], "agent.execution.id"),
        attr(nodes[1], "agent.previous_execution.id")
    );
    assert_ne!(
        attr(nodes[0], "agent.execution.id"),
        attr(nodes[1], "agent.execution.id")
    );
    assert_eq!(attr(nodes[0], "agent.operation.outcome"), "cancelled");
    assert_eq!(attr(nodes[1], "agent.operation.outcome"), "completed");
    assert_eq!(
        spans
            .iter()
            .filter(|s| s.name == "agent.run" && attr(s, "agent.run.resumed") == "true")
            .count(),
        1
    );
}

struct BadNode;
#[async_trait]
impl WorkflowNode for BadNode {
    fn kind(&self) -> &str {
        "bad"
    }
    async fn execute(
        &self,
        input: WorkflowNodeInput,
        _: &WorkflowNodeContext,
        _: &AgentServices,
    ) -> Result<WorkflowNodeResult, AgentError> {
        assert_ne!(
            input.node_id, "denied",
            "denial failed to prevent node execution"
        );
        if input.node_id == "panic" {
            panic!("private-node-panic");
        }
        let mut result = WorkflowNodeResult::completed(json!(null));
        result.routes = Some(["unknown".into()].into());
        Ok(result)
    }
}
struct Deny;
#[async_trait]
impl WorkflowNodeMiddleware for Deny {
    async fn intercept(
        &self,
        input: WorkflowNodeInput,
        context: &WorkflowNodeContext,
        next: WorkflowNodeNext<'_>,
    ) -> Result<WorkflowNodeResult, AgentError> {
        if input.node_id == "denied" {
            return Err(AgentError::Run("denied by middleware".into()));
        }
        next.run(input, context).await
    }
}
#[tokio::test]
async fn node_panics_denials_and_invalid_routes_have_failed_spans() {
    for id in ["panic", "denied", "route"] {
        let temp = tempfile::tempdir().unwrap();
        let db = database(&temp.path().join("bad.db")).await;
        let exporter = InMemorySpanExporter::default();
        let traces = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let metrics = SdkMeterProvider::default();
        let a = TestBuilder::default()
            .plugin(
                TelemetryPlugin::new("telemetry", &traces, &metrics, TelemetryConfig::default())
                    .unwrap(),
            )
            .plugin(WorkflowNodeMiddlewarePlugin::new("deny", Deny))
            .plugin(StateStorePlugin(db.clone()))
            .plugin(WorkflowNodePlugin::new("nodes", BadNode))
            .plugin(AgentLoopPlugin::new(
                DagWorkflowLoop::new(graph(vec![DagNode::new(id, "bad", json!({}))])).unwrap(),
            ))
            .build()
            .await
            .unwrap();
        let error = a.run(RunRequest::new("s", "input")).await.unwrap_err();
        if id == "denied" {
            assert_eq!(error.to_string(), "run failed: denied by middleware");
        }
        a.close().await;
        db.close().await.unwrap();
        let spans = exporter.get_finished_spans().unwrap();
        let node = spans
            .iter()
            .find(|s| s.name == "agent.workflow.node")
            .unwrap();
        assert!(matches!(node.status, Status::Error { .. }), "{id}");
        assert_eq!(node.events.events[0].name, "agent.error");
        assert!(!format!("{spans:?}").contains("private-node-panic"));
    }
}

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
#[async_trait]
impl WorkflowNode for FailAfterTool {
    fn kind(&self) -> &str {
        "fail_after_tool"
    }
    async fn execute(
        &self,
        _: WorkflowNodeInput,
        _: &WorkflowNodeContext,
        _: &AgentServices,
    ) -> Result<WorkflowNodeResult, AgentError> {
        self.0.notified().await;
        Err(AgentError::Run("sibling failed".into()))
    }
}
#[tokio::test]
async fn sibling_failure_drains_unfinished_tool_before_ending_its_node() {
    let temp = tempfile::tempdir().unwrap();
    let db = database(&temp.path().join("sibling.db")).await;
    let exporter = InMemorySpanExporter::default();
    let traces = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let metrics = SdkMeterProvider::default();
    let entered = Arc::new(Notify::new());
    let a = TestBuilder::default()
        .plugin(
            TelemetryPlugin::new("telemetry", &traces, &metrics, TelemetryConfig::default())
                .unwrap(),
        )
        .plugin(StateStorePlugin(db.clone()))
        .plugin(ChatModelPlugin::new(FixtureModel))
        .plugin(ToolProviderPlugin::new(
            "tools",
            StaticToolProvider::new(vec![ToolBinding::new(
                ToolDescriptor::new("tool", "private_tool", "", json!({"type":"object"})),
                PendingTool(entered.clone()),
            )]),
        ))
        .plugin(WorkflowNodePlugin::new("nodes", FailAfterTool(entered)))
        .plugin(AgentLoopPlugin::new(
            DagWorkflowLoop::new(graph(vec![
                DagNode::react(WorkflowStage::new("react")),
                DagNode::new("fail", "fail_after_tool", json!({})),
            ]))
            .unwrap(),
        ))
        .build()
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(3), a.run(RunRequest::new("s", "input")))
            .await
            .unwrap()
            .is_err()
    );
    a.close().await;
    db.close().await.unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    let tool = spans.iter().find(|s| s.name == "agent.tool").unwrap();
    let node = spans
        .iter()
        .find(|s| s.span_context.span_id() == tool.parent_span_id)
        .unwrap();
    let run = spans.iter().find(|s| s.name == "agent.run").unwrap();
    assert_eq!(node.name, "agent.workflow.node");
    assert_eq!(attr(tool, "agent.operation.outcome"), "unknown");
    assert_eq!(attr(node, "agent.operation.outcome"), "unknown");
    assert!(tool.end_time <= node.end_time && node.end_time <= run.end_time);
}

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
#[tokio::test]
async fn approval_resume_parents_restored_tools_and_models_to_the_new_node_span() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("approval.db");
    let exporter = InMemorySpanExporter::default();
    let traces = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let metrics = SdkMeterProvider::default();
    let mut resume = None;
    for _ in 0..2 {
        let db = database(&path).await;
        let a = TestBuilder::default()
            .plugin(
                TelemetryPlugin::new("telemetry", &traces, &metrics, TelemetryConfig::default())
                    .unwrap(),
            )
            .plugin(StateStorePlugin(db.clone()))
            .plugin(ChatModelPlugin::new(FixtureModel))
            .plugin(ToolPolicyPlugin::new("approval", RequireApproval))
            .plugin(ToolProviderPlugin::new(
                "tools",
                StaticToolProvider::new(vec![ToolBinding::new(
                    ToolDescriptor::new("tool", "private_tool", "", json!({"type":"object"})),
                    FixtureTool,
                )]),
            ))
            .plugin(AgentLoopPlugin::new(
                DagWorkflowLoop::new(graph(vec![DagNode::react(WorkflowStage::new("react"))]))
                    .unwrap(),
            ))
            .build()
            .await
            .unwrap();
        let mut request = RunRequest::new("s", "input");
        request.resume = resume.clone();
        let h = a.start(request).unwrap();
        let run_id = h.run_id;
        let result = h.wait().await.unwrap();
        if resume.is_none() {
            assert_eq!(result.status, RunStatus::AwaitingInput);
            resume = Some(ResumeRequest {
                run_id,
                response: Some(InteractionResponse {
                    interaction_id: result.interactions[0].id,
                    value: json!(true),
                }),
                ..Default::default()
            });
        } else {
            assert_eq!(result.status, RunStatus::Completed);
        }
        a.close().await;
        db.close().await.unwrap();
    }
    let spans = exporter.get_finished_spans().unwrap();
    let nodes: Vec<_> = spans
        .iter()
        .filter(|s| s.name == "agent.workflow.node")
        .collect();
    assert_eq!(nodes.len(), 2);
    assert_eq!(
        attr(nodes[0], "agent.node.invocation.id"),
        attr(nodes[1], "agent.node.invocation.id")
    );
    let tool = spans.iter().find(|s| s.name == "agent.tool").unwrap();
    assert_eq!(tool.parent_span_id, nodes[1].span_context.span_id());
    assert!(
        tool.links.links.is_empty(),
        "must not invent a link to a model from a prior execution"
    );
    assert!(spans.iter().any(|s| s.name == "agent.model" && s.parent_span_id == nodes[1].span_context.span_id()));
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
#[tokio::test]
async fn model_error_report_is_correlated_at_model_node_and_run_without_raw_text() {
    let temp = tempfile::tempdir().unwrap();
    let db = database(&temp.path().join("model-error.db")).await;
    let exporter = InMemorySpanExporter::default();
    let traces = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let metrics = SdkMeterProvider::default();
    let a = TestBuilder::default()
        .plugin(
            TelemetryPlugin::new("telemetry", &traces, &metrics, TelemetryConfig::default())
                .unwrap(),
        )
        .plugin(StateStorePlugin(db.clone()))
        .plugin(ChatModelPlugin::new(FailedModel))
        .plugin(AgentLoopPlugin::new(
            DagWorkflowLoop::new(graph(vec![DagNode::react(WorkflowStage::new("react"))])).unwrap(),
        ))
        .build()
        .await
        .unwrap();
    assert!(a.run(RunRequest::new("s", "input")).await.is_err());
    a.close().await;
    db.close().await.unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 3);
    for span in &spans {
        assert_eq!(attr(span, "error.type"), "model_fatal");
        assert!(span.events.iter().any(|event| event.name == "agent.error"));
        assert_eq!(
            span.span_context.trace_id(),
            spans[0].span_context.trace_id()
        );
    }
    assert!(!format!("{spans:?}").contains("private-provider-body"));
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
    a.close().await;
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
        .plugin(ContextPlugin::new(
            "instructions",
            FixedContext::new("private-instructions"),
        ))
        .build()
        .await
        .unwrap();
    agent
        .run(RunRequest::new("private-session", "private-input"))
        .await
        .unwrap();
    agent.close().await;
    traces.force_flush().unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    let run = spans.iter().find(|span| span.name == "agent.run").unwrap();
    let model = spans
        .iter()
        .find(|span| span.name == "agent.model")
        .unwrap();
    let requests = capture.0.lock().unwrap();
    let estimate =
        ConservativeTokenEstimator.request(&ModelRef::new("default", "default"), &requests[0]);
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
    drop(requests);
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
    agent.close().await;
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
    agent.close().await;
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
