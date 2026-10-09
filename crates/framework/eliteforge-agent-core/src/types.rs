use std::{
    any::{Any, TypeId},
    collections::{HashMap, VecDeque},
    fmt,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

pub type RunId = uuid::Uuid;

/// Opaque host data. Intentionally neither serializable nor printed by Debug.
#[derive(Clone, Default)]
pub struct Extensions(HashMap<TypeId, Arc<dyn Any + Send + Sync>>);

impl Extensions {
    pub fn insert<T: Send + Sync + 'static>(&mut self, value: T) {
        self.0.insert(TypeId::of::<T>(), Arc::new(value));
    }

    pub fn get<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.0.get(&TypeId::of::<T>())?.clone().downcast().ok()
    }
}

impl fmt::Debug for Extensions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Extensions")
            .field("count", &self.0.len())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Role {
    System,
    Developer,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    /// Framework invocation identity, distinct from the provider's protocol ID.
    /// Assigned per returned model call and retained through execution/recovery.
    /// Old transcripts without this field deserialize to nil; durable workflows
    /// restore it from their persisted ToolExecution identity before use.
    #[serde(default)]
    pub invocation_id: RunId,
    pub id: String,
    pub call_name: String,
    pub arguments: Value,
}

impl ToolCall {
    pub fn new(id: impl Into<String>, call_name: impl Into<String>, arguments: Value) -> Self {
        Self {
            invocation_id: RunId::new_v4(),
            id: id.into(),
            call_name: call_name.into(),
            arguments,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: crate::Content,
    /// Structured assistant output, retained alongside the original content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured: Option<Value>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_result: Option<ToolResultMetadata>,
    /// Adapter-owned continuation data, scoped to the originating model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_context: Option<crate::ModelMessageContext>,
}

impl Message {
    pub fn new(role: Role, content: impl Into<crate::Content>) -> Self {
        Self {
            role,
            content: content.into(),
            structured: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_result: None,
            model_context: None,
        }
    }

    pub fn user(content: impl Into<crate::Content>) -> Self {
        Self::new(Role::User, content)
    }
    pub fn system(content: impl Into<crate::Content>) -> Self {
        Self::new(Role::System, content)
    }
    pub fn assistant(content: impl Into<crate::Content>) -> Self {
        Self::new(Role::Assistant, content)
    }

    pub fn tool(call_id: impl Into<String>, content: impl Into<crate::Content>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            structured: None,
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
            tool_result: None,
            model_context: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ToolResultMetadata {
    pub is_error: bool,
    pub structured: Option<Value>,
    /// Complete original ToolOutput, when large result data was externalized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_reference: Option<Box<crate::ResourceReference>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Usage {
    #[doc(hidden)]
    pub fn add(&mut self, other: &Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDescriptor {
    /// Stable host/catalog identity; never inferred from the model call name.
    pub id: String,
    /// Model-visible function name. Must be unique within a run.
    pub call_name: String,
    pub description: String,
    pub input_schema: Value,
}

impl ToolDescriptor {
    pub fn new(
        id: impl Into<String>,
        call_name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            id: id.into(),
            call_name: call_name.into(),
            description: description.into(),
            input_schema,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub options: crate::GenerationOptions,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDescriptor>,
    #[serde(skip)]
    pub extensions: Extensions,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatResponse {
    pub content: crate::Content,
    /// Explicit public projection when a host's lossless content contains
    /// internal protocol wrappers. History never reads model_context.
    #[serde(default)]
    pub public_content: Option<crate::Content>,
    /// Only reasoning text actually returned by the provider. Signatures and
    /// opaque continuation data belong exclusively to model_context.
    #[serde(default)]
    pub reasoning: Option<String>,
    pub structured: Option<Value>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub model_context: Option<crate::ModelMessageContext>,
}

/// Optional diagnostics reported by a provider, independent of budget ledger
/// charges. Missing fields remain unknown, including during aggregation.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderMetrics {
    pub cached_input_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
}

impl ChatResponse {
    /// Preserve existing provider continuation and the summary's structured
    /// schema. A non-object opaque context cannot carry these optional metrics.
    pub fn with_provider_metrics(mut self, metrics: ProviderMetrics) -> Self {
        let context = self
            .model_context
            .get_or_insert_with(|| crate::ModelMessageContext {
                model: crate::ModelRef::new("framework", "provider-metrics"),
                protocol: "provider-metrics".into(),
                format: "framework.provider-metrics.v1".into(),
                data: serde_json::json!({}),
            });
        if let Some(data) = context.data.as_object_mut() {
            data.insert(
                "frameworkProviderMetrics".into(),
                serde_json::to_value(metrics).expect("provider metrics serialize"),
            );
        }
        self
    }
    pub fn provider_metrics(&self) -> Option<ProviderMetrics> {
        self.model_context
            .as_ref()
            .and_then(|context| context.data.get("frameworkProviderMetrics"))
            .and_then(|value| serde_json::from_value(value.clone()).ok())
    }
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: text.into().into(),
            ..Self::default()
        }
    }
    pub fn tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Self {
            tool_calls,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorCategory {
    Unavailable,
    Authentication,
    Permission,
    Quota,
    Transient,
    AccountChanged,
    Other,
}

/// Structured provider failure. Messages are raw and must pass through the
/// host's redaction policy before export, just like other model errors.
#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[error("{provider} model error: {message}")]
pub struct ProviderModelError {
    /// Opaque transport attempt identity, never a credential. Used to bind recovery to the rejected request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_attempt: Option<String>,
    pub provider: String,
    pub category: ProviderErrorCategory,
    pub http_status: Option<u16>,
    pub business_code: Option<i32>,
    pub error_type: Option<String>,
    pub model: Option<crate::ModelRef>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
pub enum ModelError {
    #[error(transparent)]
    Provider(Box<ProviderModelError>),
    #[error("model is unavailable: {0}")]
    Unavailable(String),
    #[error("operation result is unknown: {0}")]
    Indeterminate(String),
    #[error(transparent)]
    State(#[from] crate::StateError),
    #[error("transient model error: {0}")]
    Transient(String),
    #[error("model context overflow: {0}")]
    ContextOverflow(String),
    #[error("model error: {0}")]
    Fatal(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
pub enum ToolError {
    #[error("{0}")]
    Failed(String),
    #[error("tool effects are indeterminate: {0}")]
    Indeterminate(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
pub enum ConversationError {
    #[error("operation result is unknown: {0}")]
    Indeterminate(String),
    #[error("conversation budget exceeded: {0}")]
    BudgetExceeded(String),
    #[error("conversation revision conflict")]
    Conflict,
    #[error("conversation error: {0}")]
    Other(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
pub enum ContextError {
    #[error("context source refresh required: {source_revision}")]
    RefreshRequired { source_revision: String },
    #[error("operation result is unknown: {0}")]
    Indeterminate(String),
    #[error("context budget exceeded: {0}")]
    BudgetExceeded(String),
    #[error("invalid context history: {0}")]
    InvalidHistory(#[from] ConversationError),
    #[error("context error: {0}")]
    Other(String),
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("operation result is unknown: {0}")]
    Indeterminate(String),
    #[error(transparent)]
    Plugin(#[from] eliteforge_agent_plugin_core::PluginError),
    #[error(transparent)]
    Capability(#[from] crate::CapabilityError),
    #[error("required service is missing: {0}")]
    MissingService(&'static str),
    #[error("invalid configuration: {0}")]
    InvalidConfiguration(String),
    #[error("agent is closed")]
    Closed,
    #[error("session already has an active run: {0}")]
    SessionBusy(String),
    #[error("run was cancelled")]
    Cancelled,
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error(transparent)]
    ConversationStore(#[from] ConversationError),
    #[error(transparent)]
    Context(#[from] ContextError),
    #[error(transparent)]
    LongTermMemory(#[from] crate::MemoryError),
    #[error("context assembly failed: {source}")]
    ContextAssembly {
        report: Box<crate::ContextFailureReport>,
        #[source]
        source: Box<AgentError>,
    },
    #[error("tool setup error: {0}")]
    ToolSetup(String),
    #[error(transparent)]
    State(#[from] crate::StateError),
    #[error("run failed: {0}")]
    Run(String),
    /// Execution failed and its cleanup also failed. Both errors remain typed;
    /// the source chain follows the execution error.
    #[error("{primary}; cleanup also failed: {cleanup}")]
    WithCleanup {
        #[source]
        primary: Box<AgentError>,
        cleanup: Box<AgentError>,
    },
}

impl AgentError {
    /// The original execution error, without any attached cleanup failures.
    pub fn primary(&self) -> &Self {
        match self {
            Self::WithCleanup { primary, .. } => primary.primary(),
            Self::ContextAssembly { source, .. } => source.primary(),
            error => error,
        }
    }

    #[doc(hidden)]
    pub fn contains_state_error(&self) -> bool {
        match self {
            Self::State(_) | Self::Model(ModelError::State(_)) => true,
            Self::ContextAssembly { source, .. } => source.contains_state_error(),
            Self::WithCleanup { primary, cleanup } => {
                primary.contains_state_error() || cleanup.contains_state_error()
            }
            _ => false,
        }
    }
}

/// Combine an execution result with cleanup, retaining both typed errors.
pub fn finish_with_cleanup<T>(
    result: Result<T, AgentError>,
    cleanup: Result<(), AgentError>,
) -> Result<T, AgentError> {
    match (result, cleanup) {
        (result, Ok(())) => result,
        (Ok(_), Err(error)) => Err(error),
        (Err(primary), Err(cleanup)) => Err(AgentError::WithCleanup {
            primary: Box::new(primary),
            cleanup: Box::new(cleanup),
        }),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum RunStatus {
    Completed,
    AwaitingInput,
    StepLimit,
    ToolCallLimit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    pub output: String,
    pub content: crate::Content,
    pub structured: Option<Value>,
    pub interactions: Vec<crate::InteractionRequest>,
    pub status: RunStatus,
    pub usage: Usage,
    pub steps: usize,
    pub tool_calls: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_session: Option<crate::ExternalSessionRef>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolBatchBudgetPolicy {
    #[default]
    RejectBatch,
    /// Execute the ordered prefix that fits, record non-execution for the rest,
    /// then end the run with ToolCallLimit.
    ExecuteAvailable,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunLimits {
    pub max_steps: usize,
    pub max_tool_calls: usize,
    #[serde(default)]
    pub tool_batch_budget_policy: ToolBatchBudgetPolicy,
    /// Permit calls to authorized but currently hidden tools. Stage restrictions,
    /// revocations, schema validation and approval policies still apply.
    #[serde(default)]
    pub allow_unexposed_tools: bool,
    pub max_parallel_tools: usize,
    pub max_model_retries: usize,
    /// Combined main/auxiliary dispatch limit per model step. None is unlimited.
    /// Auxiliary calls reserve one remaining dispatch for the main answer.
    #[serde(default)]
    pub max_model_calls_per_step: Option<usize>,
    pub max_overflow_recoveries: usize,
    pub max_context_messages: usize,
    /// Conservative built-in estimate, not a model-specific tokenizer.
    pub max_context_tokens: usize,
    pub model_timeout: std::time::Duration,
    pub tool_timeout: std::time::Duration,
    pub run_timeout: std::time::Duration,
    pub max_total_tokens: u64,
    pub max_model_output_tokens: u32,
    pub cleanup_timeout: std::time::Duration,
    /// Maximum descendants admitted across a root execution tree. Zero disables delegation.
    #[serde(default = "default_child_runs")]
    pub max_child_runs: usize,
    /// Root depth is zero. Each descendant inherits the tighter ancestor limit.
    #[serde(default = "default_child_depth")]
    pub max_delegation_depth: usize,
}

impl Default for RunLimits {
    fn default() -> Self {
        Self {
            max_steps: 20,
            max_tool_calls: 50,
            tool_batch_budget_policy: ToolBatchBudgetPolicy::default(),
            allow_unexposed_tools: false,
            max_parallel_tools: 1,
            max_model_retries: 2,
            max_model_calls_per_step: None,
            max_overflow_recoveries: 1,
            max_context_messages: 256,
            max_context_tokens: 32_000,
            model_timeout: std::time::Duration::from_secs(120),
            tool_timeout: std::time::Duration::from_secs(60),
            run_timeout: std::time::Duration::from_secs(3600),
            max_total_tokens: 1_000_000,
            max_model_output_tokens: 4096,
            cleanup_timeout: std::time::Duration::from_secs(5),
            max_child_runs: default_child_runs(),
            max_delegation_depth: default_child_depth(),
        }
    }
}

fn default_child_runs() -> usize {
    32
}
fn default_child_depth() -> usize {
    8
}

impl RunLimits {
    #[doc(hidden)]
    pub fn validate(&self) -> Result<(), AgentError> {
        if [
            self.run_timeout,
            self.model_timeout,
            self.tool_timeout,
            self.cleanup_timeout,
        ]
        .iter()
        .any(|duration| tokio::time::Instant::now().checked_add(*duration).is_none())
        {
            return Err(AgentError::InvalidConfiguration(
                "runtime duration exceeds the supported clock range".into(),
            ));
        }
        if self.max_model_calls_per_step == Some(0)
            || self.max_model_output_tokens == 0
            || self.cleanup_timeout.is_zero()
            || self.model_timeout.is_zero()
            || self.tool_timeout.is_zero()
            || self.run_timeout.is_zero()
            || self.max_total_tokens == 0
            || self.max_steps == 0
            || self.max_parallel_tools == 0
            || self.max_context_messages == 0
            || self.max_context_tokens == 0
        {
            return Err(AgentError::InvalidConfiguration(
                "step, parallelism and context limits must be positive".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRequest {
    /// Host admission identity, shared by control, events and durable run records.
    #[serde(default)]
    pub run_id: Option<RunId>,
    pub session_id: String,
    #[serde(default)]
    pub execution_scope: Option<String>,
    #[serde(default)]
    pub conversation_branch: Option<String>,
    #[serde(default)]
    pub goal_id: Option<String>,
    pub input: crate::Content,
    pub generation: crate::GenerationOptions,
    pub limits: RunLimits,
    #[serde(skip)]
    pub extensions: Extensions,
    #[serde(skip)]
    pub resume: Option<crate::ResumeRequest>,
    /// Default ReAct phase. Custom loops pass their own phase to ModelInvoker.
    pub model_stage: String,
    #[serde(default)]
    pub model: Option<crate::ModelRef>,
}

impl RunRequest {
    pub fn scope(&self) -> &str {
        self.execution_scope.as_deref().unwrap_or("main")
    }
    pub fn occupancy_key(&self) -> String {
        format!(
            "{}:{}:{}",
            self.session_id.len(),
            self.session_id,
            self.scope()
        )
    }
    pub fn new(session_id: impl Into<String>, input: impl Into<crate::Content>) -> Self {
        Self {
            session_id: session_id.into(),
            run_id: None,
            execution_scope: None,
            conversation_branch: None,
            goal_id: None,
            input: input.into(),
            generation: crate::GenerationOptions::default(),
            limits: RunLimits::default(),
            extensions: Extensions::default(),
            model_stage: "react".into(),
            model: None,
            resume: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentEvent {
    ExternalAgent(crate::ExternalAgentEvent),
    ModelOperationStarted {
        operation: crate::ModelOperation,
        model: crate::ModelRef,
    },
    Output(Box<crate::OutputEvent>),
    ContextAssembled(crate::ContextAssemblyReport),
    ContextAssemblyFailed(crate::ContextFailureReport),
    ModelDispatched(crate::ModelDispatchReport),
    LifecycleHookFailed {
        hook_index: usize,
        phase: crate::RunLifecyclePhase,
        failure: crate::LifecycleHookFailure,
    },
    WorkflowNodeChanged {
        node_id: String,
        status: crate::WorkflowNodeStatus,
    },
    Collaboration(Box<crate::CollaborationEvent>),
    ModelSelected {
        stage: String,
        model: crate::ModelRef,
        step: usize,
        attempt: usize,
    },
    ModelUsage {
        stage: String,
        model: crate::ModelRef,
        usage: Usage,
    },
    RunStarted {
        run_id: RunId,
        session_id: String,
    },
    ModelStarted {
        step: usize,
        attempt: usize,
    },
    TextDelta(String),
    ReasoningDelta(String),
    ModelRetry {
        step: usize,
        attempt: usize,
    },
    ToolStarted {
        tool_id: String,
        call_id: String,
    },
    ToolFinished {
        tool_id: String,
        call_id: String,
        is_error: bool,
    },
    Steered(String),
    ContextRecovered,
    RunFinished {
        status: RunStatus,
    },
    RunCancelled,
    RunFailed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub schema_version: u32,
    /// Sequence numbers are unique only within this scope and run_id.
    #[serde(default)]
    pub scope: EventScope,
    pub sequence: u64,
    pub timestamp_unix_ms: u64,
    pub run_id: RunId,
    pub session_id: String,
    /// Execution identity for parallel node streams. Absent for run-wide events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_execution: Option<crate::WorkflowNodeExecution>,
    pub event: AgentEvent,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum EventScope {
    /// Durable checkpoint journal; retained across executor restarts.
    #[default]
    Journal,
    /// Best-effort stream from one execution attempt, including a resume attempt.
    Live { execution_id: RunId },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunLineage {
    pub root_run_id: RunId,
    pub parent_run_id: Option<RunId>,
    pub depth: usize,
}

#[async_trait]
pub trait ChatStream: Send + Sync {
    async fn text_delta(&self, text: &str);
    async fn reasoning_delta(&self, text: &str);
}

#[async_trait]
/// Best-effort asynchronous observations. At controlled completion, the runtime
/// drains ordinary events for at most `cleanup_timeout`, then attempts one
/// terminal callback per sink concurrently, each with an additional timeout of
/// `min(cleanup_timeout, 1 second)`. Earlier callbacks cannot run after terminal
/// dispatch. Callback completion is not guaranteed; use `Agent::wait` or a
/// durable checkpoint for authoritative execution state. Callbacks must yield
/// cooperatively so timeout/cancellation can take effect.
#[crate::extension_point(adapter = crate::EventPlugin, contribute)]
pub trait EventSink: Send + Sync {
    /// Ordered live publication, before the best-effort observer queue.
    /// Must be fast, nonblocking, do no IO and never emit another event inline.
    /// Intended for bounded transport handoff. Panics are isolated; implementations
    /// must detect their own capacity failures instead of silently dropping data.
    fn on_published(&self, _context: &RunContext, _event: &EventEnvelope) {}

    /// Called inline only for context reports and final model-dispatch metadata,
    /// while the associated span is alive. Must be fast, nonblocking and do no IO.
    /// It cannot alter execution. Ordinary events still use the bounded async queue.
    fn on_diagnostic(&self, _context: &RunContext, _event: &AgentEvent) {}

    async fn on_event(&self, context: &RunContext, event: &AgentEvent);
}

#[crate::extension_point(select, adapter = crate::AgentLoopPlugin)]
#[async_trait]
pub trait AgentLoop: Send + Sync {
    fn execution_kind(&self) -> &str {
        "CUSTOM"
    }
    /// Loops that own their history externally can run without a ConversationStore.
    fn requires_conversations(&self) -> bool {
        true
    }
    /// Whether unaddressed live steering has defined semantics for this loop.
    fn supports_steering(&self) -> bool {
        true
    }
    /// Validate loop-specific plugin dependencies before accepting any runs.
    fn validate_services(&self, _services: &AgentServices) -> Result<(), AgentError> {
        Ok(())
    }
    async fn validate_services_async(&self, services: &AgentServices) -> Result<(), AgentError> {
        self.validate_services(services)
    }
    fn supports_resume(&self) -> bool {
        false
    }
    fn requires_interactions(&self) -> bool {
        false
    }
    fn supports_tool_policy(&self) -> bool {
        false
    }
    fn requires_state(&self) -> bool {
        false
    }
    fn cooperative_cancellation(&self) -> bool {
        false
    }
    fn requires_model(&self) -> bool {
        false
    }
    async fn run(
        &self,
        request: &RunRequest,
        context: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError>;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub enum ToolEffect {
    #[default]
    Continue,
    AwaitInput,
    RequestInput(Box<crate::InteractionRequest>),
    /// Expose a subset of the tools authorized by providers for this run.
    ExposeTools(Vec<String>),
    /// Replace the visible subset without granting or revoking authority. Useful
    /// for temporary discovery expansion. IDs must already belong to this run.
    SetExposedTools(Vec<String>),
    /// Permanently revoke these tool IDs for this run. Already dispatched calls
    /// may finish; queued/future calls are checked again before executor entry.
    RevokeTools(Vec<String>),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ToolOutput {
    pub structured: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_reference: Option<Box<crate::ResourceReference>>,
    pub is_error: bool,
    pub content: crate::Content,
    pub effect: ToolEffect,
}

impl ToolOutput {
    pub fn text(content: impl Into<crate::Content>) -> Self {
        Self {
            content: content.into(),
            effect: ToolEffect::Continue,
            structured: None,
            result_reference: None,
            is_error: false,
        }
    }
    pub fn error(content: impl Into<crate::Content>) -> Self {
        Self {
            is_error: true,
            ..Self::text(content)
        }
    }
    pub fn await_input(mut self) -> Self {
        self.effect = ToolEffect::AwaitInput;
        self
    }
    pub fn request_input(mut self, request: crate::InteractionRequest) -> Self {
        self.effect = ToolEffect::RequestInput(Box::new(request));
        self
    }
    pub fn expose_tools(mut self, ids: Vec<String>) -> Self {
        self.effect = ToolEffect::ExposeTools(ids);
        self
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    /// Arguments are JSON Schema validated by the invoker before this call.
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError>;
}

#[derive(Clone)]
pub struct ToolBinding {
    pub descriptor: ToolDescriptor,
    pub executor: Arc<dyn Tool>,
    pub exposed: bool,
    /// Tools run serially unless the host explicitly opts in to parallel calls.
    pub parallel_safe: bool,
}

impl ToolBinding {
    pub fn new(descriptor: ToolDescriptor, executor: impl Tool + 'static) -> Self {
        Self {
            descriptor,
            executor: Arc::new(executor),
            exposed: true,
            parallel_safe: false,
        }
    }
    pub fn hidden(mut self) -> Self {
        self.exposed = false;
        self
    }
    pub fn parallel_safe(mut self) -> Self {
        self.parallel_safe = true;
        self
    }
}

/// Owns run-local resources. Built-in loops transfer the scope to RunContext so
/// async shutdown runs even when a tool or loop future is cancelled.
pub struct ToolLease {
    pub tools: Vec<ToolBinding>,
    #[doc(hidden)]
    pub resources: eliteforge_agent_plugin_core::ResourceScope,
}
impl ToolLease {
    pub fn new(tools: Vec<ToolBinding>) -> Self {
        Self {
            tools,
            resources: eliteforge_agent_plugin_core::ResourceScope::default(),
        }
    }
    pub fn with_cleanup(mut self, cleanup: impl FnOnce() + Send + Sync + 'static) -> Self {
        self.resources.on_cleanup(cleanup);
        self
    }
    pub fn with_shutdown<F, Fut>(mut self, shutdown: F) -> Self
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<(), String>> + Send + 'static,
    {
        self.resources.on_shutdown(shutdown);
        self
    }
    /// Custom loops may close explicitly or transfer with RunContext::own_resources.
    pub async fn close(
        &mut self,
        timeout: std::time::Duration,
    ) -> eliteforge_agent_plugin_core::ShutdownReport {
        self.resources.close_async(timeout).await
    }
}

#[crate::extension_point(adapter = crate::ToolProviderPlugin, contribute)]
#[async_trait]
pub trait ToolProvider: Send + Sync {
    /// Static inventory for Framework-wide conflict checks. Dynamic providers
    /// return None and their leased tools are validated by the runtime.
    fn descriptors(&self) -> Option<Vec<ToolDescriptor>> {
        None
    }
    async fn lease(&self, context: &RunContext) -> Result<ToolLease, AgentError>;
}

/// Services frozen at build time. Each run owns all its mutable execution state.
#[derive(Clone)]
pub struct AgentServices {
    pub external_agent: Option<Arc<dyn crate::ExternalAgent>>,
    pub extension_router: Option<Arc<dyn ExtensionRouter>>,
    pub operations: Arc<dyn crate::ModelOperations>,
    pub interactions: Option<crate::InteractionManager>,
    pub lifecycle_hooks: Vec<Arc<dyn crate::RunLifecycleHook>>,
    pub node_middleware: Vec<Arc<dyn crate::WorkflowNodeMiddleware>>,
    pub workflow_nodes: crate::WorkflowNodeRegistry,
    pub models: Option<Arc<dyn crate::ChatExecutor>>,
    pub tool_invoker: Arc<crate::ToolInvoker>,
    pub run_middleware: Vec<Arc<dyn crate::RunMiddleware>>,
    pub conversations: Arc<dyn crate::ConversationStore>,
    pub long_term_memory: Option<Arc<dyn crate::LongTermMemory>>,
    pub state: Option<Arc<dyn crate::StateStore>>,
    pub tool_policy: Vec<Arc<dyn crate::ToolPolicy>>,
    pub tools: Vec<Arc<dyn ToolProvider>>,
    pub context_sources: Vec<Arc<dyn crate::ContextContributor>>,
}

/// Stable custom contracts expose data, never Any/TypeId or borrowed service objects.
#[async_trait]
pub trait ExtensionRouter: Send + Sync {
    async fn call(
        &self,
        source: &str,
        target: &str,
        operation: &str,
        params: Value,
        run: &RunContext,
        services: &AgentServices,
    ) -> Result<Value, AgentError>;
}

#[doc(hidden)]
#[derive(Clone)]
pub struct ScopedAgentServices(pub Arc<AgentServices>);
#[doc(hidden)]
#[derive(Clone)]
pub struct ScopedRunContext(pub RunContext);
/// W3C propagation data only. Plugin logs, metrics and spans remain local.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceContext {
    pub traceparent: String,
    pub tracestate: String,
}

#[derive(Clone)]
pub struct RunContext {
    pub model: Option<crate::ModelRef>,
    #[doc(hidden)]
    pub limits: RunLimits,
    #[doc(hidden)]
    pub node_execution: Option<crate::WorkflowNodeExecution>,
    #[doc(hidden)]
    pub execution_counters: Option<Arc<crate::budget::ExecutionCounters>>,
    #[doc(hidden)]
    pub tool_dispatch_gate: Arc<tokio::sync::RwLock<()>>,
    #[doc(hidden)]
    pub tool_slots: Arc<tokio::sync::Semaphore>,
    pub run_id: RunId,
    pub session_id: String,
    pub extensions: Extensions,
    #[doc(hidden)]
    pub budget_journal: Option<Arc<dyn crate::budget::BudgetJournal>>,
    #[doc(hidden)]
    pub usage: Arc<Mutex<crate::budget::UsageLedger>>,
    #[doc(hidden)]
    pub token_ceiling: u64,
    #[doc(hidden)]
    pub resources: Arc<Mutex<Vec<eliteforge_agent_plugin_core::ResourceScope>>>,
    #[doc(hidden)]
    pub revoked_tools: Arc<Mutex<std::collections::BTreeSet<String>>>,
    #[doc(hidden)]
    pub execution_id: RunId,
    #[doc(hidden)]
    pub lineage: RunLineage,
    #[doc(hidden)]
    pub child_runs: Arc<std::sync::atomic::AtomicUsize>,
    #[doc(hidden)]
    pub max_child_runs: usize,
    #[doc(hidden)]
    pub max_child_depth: usize,
    #[doc(hidden)]
    pub children: CancellationToken,
    #[doc(hidden)]
    pub deadline: tokio::time::Instant,
    #[doc(hidden)]
    pub publication: Arc<Mutex<()>>,
    #[doc(hidden)]
    pub sequence: Arc<std::sync::atomic::AtomicU64>,
    #[doc(hidden)]
    pub observers: Option<Arc<crate::observers::ObserverDispatcher>>,
    #[doc(hidden)]
    pub cancellation: CancellationToken,
    #[doc(hidden)]
    pub steering: Arc<Mutex<SteeringState>>,
    #[doc(hidden)]
    pub events: broadcast::Sender<EventEnvelope>,
    #[doc(hidden)]
    pub sinks: Vec<Arc<dyn EventSink>>,
}

/// Host-owned live input queue, consumed at the same durable boundaries as
/// Agent::steer. `seal_if_empty` must atomically reject future admission when
/// returning no inputs. Return full typed user content, never instructions.
#[async_trait]
pub trait LiveInputSource: Send + Sync {
    async fn drain(
        &self,
        run: &RunContext,
        seal_if_empty: bool,
    ) -> Result<Vec<crate::Content>, AgentError>;
}
#[derive(Clone)]
pub struct LiveInputSourceService(pub Arc<dyn LiveInputSource>);

#[derive(Default)]
#[doc(hidden)]
pub struct SteeringState {
    pub pending: VecDeque<crate::Content>,
    pub closed: bool,
}

impl RunContext {
    /// Limits configured by the host for this run. Descendant or independent
    /// collaboration runs may tighten this policy but must not enlarge it.
    pub fn limits(&self) -> &RunLimits {
        &self.limits
    }

    /// Durable node invocation and the current attempt. Absent outside a node.
    pub fn node_execution(&self) -> Option<&crate::WorkflowNodeExecution> {
        self.node_execution.as_ref()
    }
    pub fn is_tool_revoked(&self, id: &str) -> bool {
        self.revoked_tools
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(id)
    }
    pub fn revoke_tools(&self, ids: &[String]) -> Result<(), AgentError> {
        record_revocations(&self.revoked_tools, ids)
    }

    pub fn lineage(&self) -> &RunLineage {
        &self.lineage
    }
    pub fn execution_id(&self) -> RunId {
        self.execution_id
    }
    /// Adopt a resource scope immediately after acquisition. The runtime awaits
    /// its graceful shutdown on every controlled exit; Drop executes fallbacks.
    pub fn own_resources(&self, scope: eliteforge_agent_plugin_core::ResourceScope) {
        self.resources
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(scope);
    }
    #[doc(hidden)]
    pub async fn close_resources(
        &self,
        timeout: std::time::Duration,
    ) -> eliteforge_agent_plugin_core::ShutdownReport {
        let mut scopes =
            std::mem::take(&mut *self.resources.lock().unwrap_or_else(|e| e.into_inner()));
        let mut report = eliteforge_agent_plugin_core::ShutdownReport::default();
        while let Some(mut scope) = scopes.pop() {
            report.merge(scope.close_async(timeout).await);
        }
        report
    }

    pub fn usage(&self) -> Usage {
        self.usage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .usage
            .clone()
    }
    #[doc(hidden)]
    pub async fn persist_budget(&self) -> Result<(), crate::StateError> {
        if let Some(journal) = &self.budget_journal {
            journal
                .save(self.usage.clone(), self.child_runs.clone())
                .await?;
        }
        Ok(())
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }
    pub async fn cancelled(&self) {
        self.cancellation.cancelled().await;
    }
    pub fn check_cancelled(&self) -> Result<(), AgentError> {
        if self.is_cancelled() {
            Err(AgentError::Cancelled)
        } else {
            Ok(())
        }
    }
    pub fn take_steering(&self) -> Vec<String> {
        self.take_steering_content()
            .into_iter()
            .map(|content| content.protocol_text())
            .collect()
    }
    pub fn take_steering_content(&self) -> Vec<crate::Content> {
        self.steering
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
            .drain(..)
            .collect()
    }
    #[doc(hidden)]
    pub fn pending_or_seal(&self) -> Vec<String> {
        self.pending_content_or_seal()
            .into_iter()
            .map(|content| content.protocol_text())
            .collect()
    }
    #[doc(hidden)]
    pub fn pending_content_or_seal(&self) -> Vec<crate::Content> {
        let mut state = self.steering.lock().unwrap_or_else(|e| e.into_inner());
        if state.pending.is_empty() {
            state.closed = true;
        }
        state.pending.drain(..).collect()
    }
    #[doc(hidden)]
    pub fn seal_steering(&self) {
        self.steering
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .closed = true;
    }
    pub async fn emit(&self, event: AgentEvent) {
        // Publication is live-only. Durable state/history is committed by the
        // execution strategy at semantic boundaries, never one row per delta.
        self.publish(event);
    }
    pub fn emit_output(&self, event: crate::OutputEvent) {
        self.publish(AgentEvent::Output(Box::new(event)));
    }
    #[doc(hidden)]
    pub fn publish(&self, event: AgentEvent) {
        if matches!(
            event,
            AgentEvent::ContextAssembled(_)
                | AgentEvent::ContextAssemblyFailed(_)
                | AgentEvent::ModelDispatched(_)
        ) {
            for sink in &self.sinks {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    sink.on_diagnostic(self, &event)
                }));
            }
        }
        let _publication = self.publication.lock().unwrap_or_else(|e| e.into_inner());
        let envelope = self.envelope(event.clone());
        for sink in &self.sinks {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                sink.on_published(self, &envelope)
            }));
        }
        let _ = self.events.send(envelope);
        if let Some(observers) = &self.observers {
            observers.enqueue(event);
        }
    }
    #[doc(hidden)]
    pub fn envelope(&self, event: AgentEvent) -> EventEnvelope {
        EventEnvelope {
            schema_version: 1,
            scope: EventScope::Live {
                execution_id: self.execution_id,
            },
            sequence: self
                .sequence
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            timestamp_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(u64::MAX as u128) as u64,
            run_id: self.run_id,
            session_id: self.session_id.clone(),
            node_execution: self.node_execution().cloned(),
            event,
        }
    }
    pub fn dropped_observations(&self) -> u64 {
        self.observers.as_ref().map_or(0, |o| o.dropped())
    }
}

#[async_trait]
impl ChatStream for RunContext {
    async fn text_delta(&self, text: &str) {
        self.emit(AgentEvent::TextDelta(text.to_owned())).await;
    }
    async fn reasoning_delta(&self, text: &str) {
        self.emit(AgentEvent::ReasoningDelta(text.to_owned())).await;
    }
}

#[doc(hidden)]
pub fn record_revocations(
    registry: &Mutex<std::collections::BTreeSet<String>>,
    ids: &[String],
) -> Result<(), AgentError> {
    let mut revoked = registry.lock().unwrap_or_else(|e| e.into_inner());
    if ids.iter().any(|id| id.is_empty() || id.len() > 1024) {
        return Err(AgentError::InvalidConfiguration(
            "revoked tool IDs must be nonempty and at most 1024 bytes".into(),
        ));
    }
    let additional: std::collections::BTreeSet<_> = ids
        .iter()
        .filter(|id| !revoked.contains(*id))
        .cloned()
        .collect();
    if revoked.len().saturating_add(additional.len()) > 4096 {
        return Err(AgentError::Run(
            "run tool revocation capacity exceeded".into(),
        ));
    }
    revoked.extend(additional);
    Ok(())
}
