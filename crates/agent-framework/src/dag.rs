//! Versioned DAG definitions and durable, plugin-extensible execution.
//!
//! Graph indices are private compilation details. Checkpoints, routes and
//! reconciliation always use the stable IDs supplied by the definition.
use crate::*;
use async_trait::async_trait;
use petgraph::{algo::toposort, graph::DiGraph};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::Mutex;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum WorkflowJoin {
    /// Wait for all predecessors to settle, then consume selected edges only.
    #[default]
    Selected,
    /// Execute only if every incoming edge was selected; otherwise skip.
    All,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DagNode {
    pub id: String,
    pub kind: String,
    #[serde(default)]
    pub config: Value,
    #[serde(default)]
    pub join: WorkflowJoin,
}
impl DagNode {
    pub fn new(id: impl Into<String>, kind: impl Into<String>, config: Value) -> Self {
        Self {
            id: id.into(),
            kind: kind.into(),
            config,
            join: WorkflowJoin::Selected,
        }
    }
    pub fn react(stage: WorkflowStage) -> Self {
        Self::new(
            stage.name.clone(),
            "framework.react",
            serde_json::to_value(stage).expect("serializable stage"),
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DagEdge {
    pub from: String,
    pub to: String,
}
impl DagEdge {
    pub fn new(from: impl Into<String>, to: impl Into<String>) -> Self {
        Self {
            from: from.into(),
            to: to.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DagWorkflowDefinition {
    pub id: String,
    pub version: u32,
    pub nodes: Vec<DagNode>,
    pub edges: Vec<DagEdge>,
    pub max_parallel_nodes: usize,
}
impl DagWorkflowDefinition {
    /// A sequential workflow is a chain of ordinary DAG ReAct nodes.
    pub fn from_stages(definition: WorkflowDefinition) -> Result<Self, AgentError> {
        definition.validate()?;
        let nodes: Vec<_> = definition.stages.into_iter().map(DagNode::react).collect();
        let edges = nodes
            .windows(2)
            .map(|p| DagEdge::new(&p[0].id, &p[1].id))
            .collect();
        Ok(Self {
            id: definition.id,
            version: definition.version,
            nodes,
            edges,
            max_parallel_nodes: 1,
        })
    }
    pub fn validate(&self) -> Result<(), AgentError> {
        self.compile().map(|_| ())
    }
    fn compile(&self) -> Result<Vec<String>, AgentError> {
        if self.id.trim().is_empty()
            || self.version == 0
            || self.nodes.is_empty()
            || self.max_parallel_nodes == 0
        {
            return Err(invalid(
                "DAG requires identity, version, nodes and positive concurrency",
            ));
        }
        let mut graph = DiGraph::<String, ()>::new();
        let mut indices = BTreeMap::new();
        for node in &self.nodes {
            if node.id.trim().is_empty()
                || node.kind.trim().is_empty()
                || indices.contains_key(&node.id)
            {
                return Err(invalid(
                    "DAG node IDs must be nonempty and unique; kinds must be nonempty",
                ));
            }
            indices.insert(node.id.clone(), graph.add_node(node.id.clone()));
        }
        let mut seen = BTreeSet::new();
        for edge in &self.edges {
            let from = indices
                .get(&edge.from)
                .ok_or_else(|| invalid("unknown DAG edge source"))?;
            let to = indices
                .get(&edge.to)
                .ok_or_else(|| invalid("unknown DAG edge destination"))?;
            if !seen.insert((&edge.from, &edge.to)) {
                return Err(invalid("duplicate DAG edge"));
            }
            graph.add_edge(*from, *to, ());
        }
        Ok(toposort(&graph, None)
            .map_err(|_| invalid("workflow graph contains a cycle"))?
            .into_iter()
            .map(|i| graph[i].clone())
            .collect())
    }
    pub(crate) fn validate_result(
        &self,
        id: &str,
        result: &WorkflowNodeResult,
    ) -> Result<(), AgentError> {
        if let Some(routes) = &result.routes {
            for to in routes {
                if !self.edges.iter().any(|e| e.from == id && &e.to == to) {
                    return Err(invalid(format!("node {id} selected a non-successor {to}")));
                }
            }
        }
        if result.status == RunStatus::Completed && !result.interactions.is_empty() {
            return Err(invalid("completed node cannot retain pending interactions"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowNodeInput {
    pub node_id: String,
    /// Stable for this attempt, including resume; a fresh explicit retry changes it.
    pub invocation_id: RunId,
    pub config: Value,
    pub request: RunRequest,
    /// Only successful predecessors whose edge selected this node, in ID order.
    pub predecessors: BTreeMap<String, Value>,
    pub predecessor_content: BTreeMap<String, Content>,
    pub checkpoint: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowNodeResult {
    pub value: Value,
    /// Preserve media and other typed output alongside the routing value.
    #[serde(default)]
    pub content: Option<Content>,
    /// None selects all outgoing edges; an empty set selects none.
    pub routes: Option<BTreeSet<String>>,
    pub status: RunStatus,
    pub interactions: Vec<InteractionRequest>,
    pub steps: usize,
    pub tool_calls: usize,
}
impl WorkflowNodeResult {
    pub fn completed(value: Value) -> Self {
        Self {
            value,
            content: None,
            routes: None,
            status: RunStatus::Completed,
            interactions: Vec::new(),
            steps: 0,
            tool_calls: 0,
        }
    }
}

/// Node implementations are trusted in-process plugins. They must yield during
/// async work and must not detach work from the invocation's lifetime.
#[async_trait]
pub trait WorkflowNode: Send + Sync {
    fn kind(&self) -> &str;
    /// Host UIs can use this schema to render configuration forms. Semantic
    /// checks that JSON Schema cannot express belong in `validate`.
    fn configuration_schema(&self) -> Value {
        Value::Bool(true)
    }
    fn validate(&self, _config: &Value) -> Result<(), AgentError> {
        Ok(())
    }
    fn requires_model(&self) -> bool {
        false
    }
    /// Declare this only for nodes that do not execute tools, or enforce every
    /// installed tool policy before dispatch (including resume).
    fn supports_tool_policy(&self) -> bool {
        false
    }
    /// True only when execute can safely reconcile its own persisted intent.
    /// Default nodes require explicit RecordNode/RetryNode after interruption.
    fn supports_resume(&self) -> bool {
        false
    }
    async fn execute(
        &self,
        input: WorkflowNodeInput,
        context: &WorkflowNodeContext,
        services: &AgentServices,
    ) -> Result<WorkflowNodeResult, AgentError>;
}

#[derive(Clone)]
pub struct WorkflowNodeRegistry(BTreeMap<String, Arc<dyn WorkflowNode>>);
impl WorkflowNodeRegistry {
    pub fn new(nodes: Vec<Arc<dyn WorkflowNode>>) -> Result<Self, AgentError> {
        let mut registry: BTreeMap<String, Arc<dyn WorkflowNode>> = BTreeMap::new();
        registry.insert("framework.react".into(), Arc::new(ReactWorkflowNode));
        for node in nodes {
            if node.kind().trim().is_empty() || registry.contains_key(node.kind()) {
                return Err(invalid("workflow node kinds must be nonempty and unique"));
            }
            compile_schema(&node.configuration_schema())?;
            registry.insert(node.kind().to_owned(), node);
        }
        Ok(Self(registry))
    }
    pub fn get(&self, kind: &str) -> Option<Arc<dyn WorkflowNode>> {
        self.0.get(kind).cloned()
    }
    pub fn kinds(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }
    pub fn descriptors(&self) -> Vec<WorkflowNodeDescriptor> {
        self.0
            .values()
            .map(|node| WorkflowNodeDescriptor {
                kind: node.kind().to_owned(),
                configuration_schema: node.configuration_schema(),
                requires_model: node.requires_model(),
                supports_resume: node.supports_resume(),
                supports_tool_policy: node.supports_tool_policy(),
            })
            .collect()
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowNodeDescriptor {
    pub kind: String,
    pub configuration_schema: Value,
    pub requires_model: bool,
    pub supports_resume: bool,
    pub supports_tool_policy: bool,
}
impl Default for WorkflowNodeRegistry {
    fn default() -> Self {
        Self::new(Vec::new()).expect("builtin node")
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum WorkflowNodeStatus {
    Pending,
    Running,
    Suspended,
    Succeeded,
    Skipped,
    Failed,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowNodeExecution {
    pub workflow_id: String,
    pub workflow_version: u32,
    pub node_id: String,
    /// Stable across continuation; an explicit RetryNode allocates a new ID.
    pub invocation_id: RunId,
    /// Fresh for each dispatch, even within the same process.
    pub attempt_id: RunId,
    pub execution_id: RunId,
    pub previous_attempt_id: Option<RunId>,
    pub previous_execution_id: Option<RunId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowNodeState {
    /// Persisted with admission before middleware or user code is entered.
    #[serde(default)]
    pub last_execution: Option<WorkflowNodeExecution>,
    pub status: WorkflowNodeStatus,
    pub invocation_id: RunId,
    pub checkpoint: Option<Value>,
    /// Optional transcript recorded explicitly by custom nodes. Embedded ReAct
    /// keeps its transcript in its own checkpoint instead of duplicating it here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<ConversationSnapshot>,
    pub result: Option<WorkflowNodeResult>,
    pub error: Option<String>,
}
impl Default for WorkflowNodeState {
    fn default() -> Self {
        Self {
            last_execution: None,
            status: WorkflowNodeStatus::Pending,
            invocation_id: RunId::new_v4(),
            checkpoint: None,
            conversation: None,
            result: None,
            error: None,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DagWorkflowState {
    pub schema_version: u32,
    pub definition: DagWorkflowDefinition,
    pub request: RunRequest,
    pub nodes: BTreeMap<String, WorkflowNodeState>,
    pub budget_usage: Usage,
    pub model_steps: usize,
    pub tool_calls: usize,
    pub child_runs: usize,
    pub revoked_tools: BTreeSet<String>,
    pub result: Option<RunResult>,
}

#[derive(Clone)]
pub struct WorkflowNodeContext {
    pub run: RunContext,
    pub resume: Option<ResumeRequest>,
    node_id: String,
    journal: Arc<DagJournal>,
}
impl WorkflowNodeContext {
    /// Custom-node history is independent of other nodes and survives resume.
    /// Recording is explicit: a raw ToolInvoker call does not append messages.
    pub async fn conversation_snapshot(&self) -> Result<ConversationSnapshot, AgentError> {
        let state = self.journal.state().await?;
        let node = state.nodes.get(&self.node_id).expect("compiled node");
        Ok(node
            .conversation
            .clone()
            .unwrap_or_else(|| self.empty_conversation(node.invocation_id)))
    }
    pub async fn append_conversation(
        &self,
        revision: u64,
        messages: &[Message],
    ) -> Result<(), AgentError> {
        self.record_conversation(None, revision, messages).await
    }
    /// Commit node state and its complete interaction batch in one parent
    /// checkpoint. A failed/unknown write stops the journal, never replays a tool.
    pub async fn save_with_conversation(
        &self,
        checkpoint: Value,
        revision: u64,
        messages: &[Message],
    ) -> Result<(), AgentError> {
        self.record_conversation(Some(checkpoint), revision, messages)
            .await
    }
    fn empty_conversation(&self, invocation: RunId) -> ConversationSnapshot {
        ConversationSnapshot::new(
            ConversationKey::new(&self.run.session_id)
                .branch(format!("{}/{}", self.run.run_id, self.node_id)),
            invocation.to_string(),
            Vec::new(),
        )
    }
    async fn record_conversation(
        &self,
        checkpoint: Option<Value>,
        revision: u64,
        messages: &[Message],
    ) -> Result<(), AgentError> {
        self.run.check_cancelled()?;
        self.journal
            .change(|state, _| {
                let node = state.nodes.get_mut(&self.node_id).expect("compiled node");
                if node.status != WorkflowNodeStatus::Running
                    || node.last_execution.as_ref().map(|n| n.attempt_id)
                        != self.run.node_execution().map(|n| n.attempt_id)
                {
                    return Err(StateError::Conflict.into());
                }
                let mut conversation = node
                    .conversation
                    .clone()
                    .unwrap_or_else(|| self.empty_conversation(node.invocation_id));
                if conversation.revision != revision {
                    return Err(ConversationError::Conflict.into());
                }
                conversation.messages.extend_from_slice(messages);
                validate_messages(&conversation.messages)?;
                conversation.revision = conversation.messages.len() as u64;
                node.conversation = Some(conversation);
                if let Some(checkpoint) = checkpoint {
                    node.checkpoint = Some(checkpoint);
                }
                Ok(None)
            })
            .await
    }

    /// Persist node-local serializable state before effects. A failed save must
    /// stop execution; the journal rejects all further writes on uncertain IO.
    pub async fn save(&self, checkpoint: Value) -> Result<(), AgentError> {
        self.journal
            .change(|state, _| {
                state
                    .nodes
                    .get_mut(&self.node_id)
                    .expect("compiled node")
                    .checkpoint = Some(checkpoint);
                Ok(None)
            })
            .await
    }
}

struct DagJournal {
    store: Arc<dyn StateStore>,
    lease: ExecutionLease,
    current: Mutex<Checkpoint>,
    poisoned: AtomicBool,
    context: RunContext,
}
impl DagJournal {
    async fn state(&self) -> Result<DagWorkflowState, AgentError> {
        decode(self.current.lock().await.state.clone())
    }
    async fn change<F>(&self, change: F) -> Result<(), AgentError>
    where
        F: FnOnce(&mut DagWorkflowState, &mut Checkpoint) -> Result<Option<AgentEvent>, AgentError>
            + Send,
    {
        let mut current = self.current.lock().await;
        if self.poisoned.load(Ordering::Acquire) {
            return Err(
                StateError::Backend("DAG journal has an uncertain write outcome".into()).into(),
            );
        }
        let mut next = current.clone();
        let mut state: DagWorkflowState = decode(next.state.clone())?;
        let before: BTreeMap<_, _> = state
            .nodes
            .iter()
            .map(|(id, n)| (id.clone(), n.status))
            .collect();
        let event = change(&mut state, &mut next)?;
        let mut events: Vec<_> = event.into_iter().collect();
        for (id, node) in &state.nodes {
            if before.get(id) != Some(&node.status) && !events.iter().any(|event| matches!(event, AgentEvent::WorkflowNodeChanged {node_id,..} if node_id == id)) {
                events.push(node_event(id,node.status));
            }
        }
        state.budget_usage = self
            .context
            .usage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .including_reservations();
        state.child_runs = self.context.child_runs.load(Ordering::Acquire);
        if let Some(counters) = &self.context.execution_counters {
            state.model_steps = counters.steps.load(Ordering::Acquire);
            state.tool_calls = counters.tools.load(Ordering::Acquire);
        }
        state.revoked_tools = self
            .context
            .revoked_tools
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        next.state = encode(&state)?;
        for event in &events {
            next.events.push(EventEnvelope {
                scope: EventScope::Journal,
                schema_version: 1,
                run_id: next.run_id,
                session_id: next.session_id.clone(),
                node_execution: self.context.node_execution().cloned(),
                sequence: next.events.last().map_or(1, |e| e.sequence + 1),
                timestamp_unix_ms: unix_time_ms(),
                event: event.clone(),
            });
        }
        // If this future is dropped during commit, no other caller may advance
        // the stale in-memory revision. Clear poison only after a confirmed write.
        self.poisoned.store(true, Ordering::Release);
        *current = self
            .store
            .commit(&self.lease, current.revision, next)
            .await?;
        self.poisoned.store(false, Ordering::Release);
        for event in events {
            self.context.emit(event).await;
        }
        Ok(())
    }
}
#[async_trait]
impl crate::budget::BudgetJournal for DagJournal {
    async fn save(
        &self,
        _: Arc<std::sync::Mutex<crate::budget::UsageLedger>>,
        _: Arc<std::sync::atomic::AtomicUsize>,
    ) -> Result<(), StateError> {
        self.change(|_, _| Ok(None)).await.map_err(|e| match e {
            AgentError::State(e) => e,
            _ => StateError::Invalid(e.to_string()),
        })
    }
}

pub struct DagWorkflowLoop {
    definition: DagWorkflowDefinition,
    order: Vec<String>,
    lease_duration: Duration,
}
impl DagWorkflowLoop {
    pub fn new(definition: DagWorkflowDefinition) -> Result<Self, AgentError> {
        let order = definition.compile()?;
        Ok(Self {
            definition,
            order,
            lease_duration: Duration::from_secs(30),
        })
    }
    pub fn with_lease_duration(mut self, duration: Duration) -> Result<Self, AgentError> {
        if duration < Duration::from_millis(30) {
            return Err(invalid("DAG lease must be at least 30 milliseconds"));
        }
        self.lease_duration = duration;
        Ok(self)
    }
    fn validate_checkpoint(
        &self,
        cp: &Checkpoint,
        request: &RunRequest,
        context: &RunContext,
    ) -> Result<DagWorkflowState, AgentError> {
        cp.validate()?;
        let state: DagWorkflowState = decode(cp.state.clone())?;
        if state.schema_version != 1 {
            return Err(StateError::UnsupportedVersion.into());
        }
        if cp.run_id != context.run_id
            || cp.session_id != request.session_id
            || cp.loop_id != self.definition.id
            || cp.loop_version != self.definition.version
            || state.definition != self.definition
            || state.request.session_id != request.session_id
            || state.nodes.keys().cloned().collect::<BTreeSet<_>>()
                != self.order.iter().cloned().collect()
        {
            return Err(invalid("checkpoint does not match installed DAG and run"));
        }
        state.request.limits.validate()?;
        state.request.generation.validate()?;
        for (id, node) in &state.nodes {
            if node.invocation_id.is_nil() {
                return Err(invalid("invalid DAG invocation identity"));
            }
            if node.status == WorkflowNodeStatus::Succeeded
                && node
                    .result
                    .as_ref()
                    .is_none_or(|r| r.status != RunStatus::Completed)
            {
                return Err(invalid(
                    "successful DAG node is missing its completed result",
                ));
            }
            if node.status == WorkflowNodeStatus::Suspended
                && node
                    .result
                    .as_ref()
                    .is_none_or(|r| r.status == RunStatus::Completed)
            {
                return Err(invalid(
                    "suspended DAG node is missing its suspension result",
                ));
            }
            if let Some(result) = &node.result {
                self.definition.validate_result(id, result)?;
            }
        }
        Ok(state)
    }
}

#[async_trait]
impl AgentLoop for DagWorkflowLoop {
    // A DAG may have several active nodes. Untargeted steering must not be
    // consumed by whichever ReAct node happens to win a race. Use interactions.
    fn supports_steering(&self) -> bool {
        false
    }
    fn supports_resume(&self) -> bool {
        true
    }
    fn supports_tool_policy(&self) -> bool {
        true
    }
    fn requires_state(&self) -> bool {
        true
    }
    fn cooperative_cancellation(&self) -> bool {
        true
    }
    fn validate_services(&self, services: &AgentServices) -> Result<(), AgentError> {
        for node in &self.definition.nodes {
            let implementation = services
                .workflow_nodes
                .get(&node.kind)
                .ok_or_else(|| invalid(format!("missing workflow node kind: {}", node.kind)))?;
            implementation.validate(&node.config)?;
            if !compile_schema(&implementation.configuration_schema())?.is_valid(&node.config) {
                return Err(invalid(format!(
                    "configuration does not match node schema: {}",
                    node.id
                )));
            }
            if node.kind == "framework.react" && services.interactions.is_none() {
                return Err(AgentError::MissingService("InteractionStore"));
            }
            if !services.tool_policy.is_empty() && !implementation.supports_tool_policy() {
                return Err(invalid(format!(
                    "node kind does not enforce installed tool policies: {}",
                    node.kind
                )));
            }
            if implementation.requires_model() && services.models.is_none() {
                return Err(AgentError::MissingService("Model"));
            }
        }
        Ok(())
    }
    async fn run(
        &self,
        request: &RunRequest,
        context: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        self.validate_services(services)?;
        let store = services
            .state
            .clone()
            .ok_or(AgentError::MissingService("StateStore"))?;
        let cp = if let Some(resume) = &request.resume {
            store.load(resume.run_id).await?
        } else {
            let previous = store.latest(&request.session_id).await?;
            let mut saved = request.clone();
            saved.resume = None;
            saved.extensions = Extensions::default();
            let state = DagWorkflowState {
                schema_version: 1,
                definition: self.definition.clone(),
                request: saved,
                nodes: self
                    .order
                    .iter()
                    .map(|id| (id.clone(), WorkflowNodeState::default()))
                    .collect(),
                budget_usage: Usage::default(),
                model_steps: 0,
                tool_calls: 0,
                child_runs: 0,
                revoked_tools: BTreeSet::new(),
                result: None,
            };
            let mut cp = Checkpoint::new(
                context.run_id,
                &request.session_id,
                &self.definition.id,
                self.definition.version,
                encode(&state)?,
            );
            if let Some(previous) = &previous {
                cp.transcript = previous.transcript.clone();
            }
            cp.transcript.push(Message::user(&request.input));
            store.create(cp, previous.map(|p| p.run_id)).await?
        };
        let state = self.validate_checkpoint(&cp, request, context)?;
        if cp.status.is_terminal() {
            return terminal_result(cp.status, state, request);
        }
        let lease = store
            .claim(context.run_id, RunId::new_v4(), self.lease_duration)
            .await?;
        let guard = crate::model_execution::WorkflowLease::new(
            store.clone(),
            lease.clone(),
            self.lease_duration,
            request.limits.cleanup_timeout,
            context.clone(),
        );
        let result = async {
            let cp = store.load(context.run_id).await?;
            let state = self.validate_checkpoint(&cp, request, context)?;
            if cp.status.is_terminal() {
                return terminal_result(cp.status, state, request);
            }
            let mut run = context.clone();
            run.execution_counters = Some(Arc::new(crate::budget::ExecutionCounters {
                steps: std::sync::atomic::AtomicUsize::new(state.model_steps),
                tools: std::sync::atomic::AtomicUsize::new(state.tool_calls),
            }));
            run.tool_slots = Arc::new(tokio::sync::Semaphore::new(
                request
                    .limits
                    .max_parallel_tools
                    .min(state.request.limits.max_parallel_tools),
            ));
            run.token_ceiling = run.token_ceiling.min(state.request.limits.max_total_tokens);
            run.max_child_runs = run.max_child_runs.min(state.request.limits.max_child_runs);
            run.max_child_depth = run
                .max_child_depth
                .min(state.request.limits.max_delegation_depth);
            run.deadline = run
                .deadline
                .min(tokio::time::Instant::now() + state.request.limits.run_timeout);
            run.usage.lock().unwrap_or_else(|e| e.into_inner()).usage = state.budget_usage.clone();
            run.child_runs.fetch_max(state.child_runs, Ordering::AcqRel);
            run.revoke_tools(&state.revoked_tools.into_iter().collect::<Vec<_>>())?;
            let journal = Arc::new(DagJournal {
                store,
                lease,
                current: Mutex::new(cp),
                poisoned: AtomicBool::new(false),
                context: run.clone(),
            });
            run.budget_journal = Some(journal.clone());
            self.execute(request, run, services, journal).await
        }
        .await;
        finish_with_cleanup(result, guard.close().await.map_err(Into::into))
    }
}

fn terminal_result(
    status: ExecutionStatus,
    state: DagWorkflowState,
    request: &RunRequest,
) -> Result<RunResult, AgentError> {
    if request.resume.as_ref().is_some_and(|r| {
        !r.resolutions.is_empty()
            || r.response.as_ref().is_some_and(|response| {
                !state
                    .nodes
                    .values()
                    .any(|n| embedded_workflow(n).is_some_and(|s| s.answers.contains(response)))
            })
    }) {
        return Err(invalid("terminal DAG cannot accept new resolutions"));
    }
    match status {
        ExecutionStatus::Cancelled => Err(AgentError::Cancelled),
        ExecutionStatus::Failed => Err(AgentError::Run("persisted DAG failed".into())),
        _ => state
            .result
            .ok_or_else(|| invalid("terminal DAG is missing its result")),
    }
}
fn invalid(message: impl Into<String>) -> AgentError {
    AgentError::InvalidConfiguration(message.into())
}
fn encode<T: Serialize>(value: &T) -> Result<Value, AgentError> {
    serde_json::to_value(value)
        .map_err(|_| StateError::Invalid("cannot encode DAG state".into()).into())
}
fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, AgentError> {
    serde_json::from_value(value)
        .map_err(|_| StateError::Invalid("cannot decode DAG state".into()).into())
}

impl DagWorkflowLoop {
    async fn execute(
        &self,
        request: &RunRequest,
        run: RunContext,
        services: &AgentServices,
        journal: Arc<DagJournal>,
    ) -> Result<RunResult, AgentError> {
        let mut resumes = BTreeMap::<String, ResumeRequest>::new();
        if request.resume.is_some()
            && let Some(manager) = &services.interactions
        {
            for (id, node) in journal.state().await?.nodes {
                if node.status != WorkflowNodeStatus::Suspended {
                    continue;
                }
                let interactions = node
                    .result
                    .as_ref()
                    .map(|r| r.interactions.clone())
                    .unwrap_or_default();
                for interaction in interactions {
                    match manager.0.get(&run.session_id, interaction.id).await {
                        Ok(record) if record.response.is_some() => {
                            if record.run_id != run.run_id {
                                return Err(invalid("interaction belongs to another run"));
                            }
                            resumes.entry(id.clone()).or_insert_with(|| ResumeRequest {
                                run_id: run.run_id,
                                response: record.response,
                                resolutions: vec![],
                            });
                        }
                        Ok(_) | Err(StateError::NotFound) => {}
                        Err(error) => return Err(error.into()),
                    }
                }
            }
        }
        journal.change(|state, cp| {
            if let Some(resume) = &request.resume {
                let mut resolved = BTreeSet::new();
                for resolution in &resume.resolutions {
                    match resolution {
                        CallResolution::RecordNode { node_id, invocation_id, result } => {
                            if !resolved.insert(node_id.clone()) { return Err(invalid("duplicate node reconciliation")); }
                            self.definition.validate_result(node_id, result)?;
                            if result.status != RunStatus::Completed { return Err(invalid("RecordNode requires a completed result")); }
                            let node = reconcile_node(state, node_id, *invocation_id)?;
                            node.status = WorkflowNodeStatus::Succeeded;
                            node.result = Some(result.clone()); node.error = None;
                        }
                        CallResolution::RetryNode { node_id, invocation_id } => {
                            if !resolved.insert(node_id.clone()) { return Err(invalid("duplicate node reconciliation")); }
                            let node = reconcile_node(state, node_id, *invocation_id)?;
                            let last_execution = node.last_execution.clone();
                            *node = WorkflowNodeState { last_execution, ..Default::default() };
                        }
                        CallResolution::Record { invocation_id, .. } | CallResolution::Retry { invocation_id } => {
                            let owners: Vec<_> = state.nodes.iter().filter(|(_, n)| embedded_workflow(n).is_some_and(|s|
                                matches!(s.cursor, WorkflowCursor::Tools { executions, .. } if executions.iter().any(|e| e.invocation_id == *invocation_id && e.state == ToolExecutionState::Started))
                            )).map(|(id, _)| id.clone()).collect();
                            if owners.len() != 1 { return Err(invalid("tool reconciliation must identify one pending DAG invocation")); }
                            resumes.entry(owners[0].clone()).or_insert_with(|| ResumeRequest { run_id: run.run_id, ..Default::default() }).resolutions.push(resolution.clone());
                        }
                    }
                }
                if let Some(response) = &resume.response {
                    let owners: Vec<_> = state.nodes.iter().filter(|(_, n)|
                        n.result.as_ref().is_some_and(|r| r.interactions.iter().any(|i| i.id == response.interaction_id))
                        || embedded_workflow(n).is_some_and(|s| s.interactions.iter().any(|i| i.id == response.interaction_id))
                    ).map(|(id, _)| id.clone()).collect();
                    if owners.len() != 1 { return Err(invalid("answer must identify one pending DAG interaction")); }
                    resumes.entry(owners[0].clone()).or_insert_with(|| ResumeRequest { run_id: run.run_id, ..Default::default() }).response = Some(response.clone());
                }
                if resolved.iter().any(|id| resumes.contains_key(id)) {
                    return Err(invalid("cannot combine node reconciliation with an answer or tool reconciliation for that node"));
                }
            }
            cp.status = ExecutionStatus::Running;
            Ok(None)
        }).await?;

        let graph = Arc::new(self.definition.clone());
        let mut tasks = tokio::task::JoinSet::new();
        let mut active = BTreeSet::<String>::new();
        let execution = async {
            loop {
                run.check_cancelled()?;
                if tokio::time::Instant::now() >= run.deadline { return Err(AgentError::Run("DAG run deadline exceeded".into())); }
                let state = journal.state().await?;
                let unresolved = state.nodes.iter().any(|(id, n)| (n.status == WorkflowNodeStatus::Failed && !resumes.contains_key(id)) ||
                    (n.status == WorkflowNodeStatus::Running && !active.contains(id) && !services.workflow_nodes.get(&self.node(id).kind).expect("validated kind").supports_resume()));
                let paused = state.nodes.iter().any(|(id, n)| n.status == WorkflowNodeStatus::Suspended && !resumes.contains_key(id));
                if !unresolved {
                    for id in &self.order {
                        if active.len() >= self.definition.max_parallel_nodes { break; }
                        let node = &state.nodes[id];
                        if active.contains(id) { continue; }
                        let start = match node.status {
                            WorkflowNodeStatus::Running => true,
                            WorkflowNodeStatus::Failed => resumes.contains_key(id),
                            WorkflowNodeStatus::Suspended => resumes.contains_key(id),
                            WorkflowNodeStatus::Pending if paused => false,
                            WorkflowNodeStatus::Pending => match self.readiness(id, &state) {
                                Ready::Wait => false,
                                Ready::Run => true,
                                Ready::Skip => {
                                    journal.change(|s, _| { s.nodes.get_mut(id).unwrap().status = WorkflowNodeStatus::Skipped; Ok(Some(node_event(id, WorkflowNodeStatus::Skipped))) }).await?;
                                    continue;
                                }
                            },
                            _ => false,
                        };
                        if !start { continue; }
                        run.check_cancelled()?;
                        let definition = self.node(id);
                        let implementation = services.workflow_nodes.get(&definition.kind).expect("validated node kind");
                        let input = WorkflowNodeInput { node_id: id.clone(), invocation_id: node.invocation_id,
                            config: definition.config.clone(), request: state.request.clone(),
                            predecessors: self.definition.edges.iter().filter(|e| e.to == *id && selected(&state.nodes[&e.from], id))
                                .map(|e| (e.from.clone(), state.nodes[&e.from].result.as_ref().unwrap().value.clone())).collect(),
                            predecessor_content: self.definition.edges.iter().filter(|e| e.to == *id && selected(&state.nodes[&e.from], id))
                                .filter_map(|e| state.nodes[&e.from].result.as_ref().unwrap().content.clone().map(|c| (e.from.clone(), c))).collect(),
                            checkpoint: node.checkpoint.clone() };
                        // Admission is durable before executor entry. No effect can
                        // occur on a failed or uncertain intent write.
                        let identity = WorkflowNodeExecution {
                            workflow_id: self.definition.id.clone(), workflow_version: self.definition.version,
                            node_id: id.clone(), invocation_id: node.invocation_id,
                            attempt_id: RunId::new_v4(), execution_id: run.execution_id(),
                            previous_attempt_id: node.last_execution.as_ref().map(|e| e.attempt_id),
                            previous_execution_id: node.last_execution.as_ref().map(|e| e.execution_id),
                        };
                        journal.change(|s, _| {
                            let node = s.nodes.get_mut(id).unwrap();
                            node.status = WorkflowNodeStatus::Running;
                            node.last_execution = Some(identity.clone());
                            Ok(Some(node_event(id, WorkflowNodeStatus::Running)))
                        }).await?;
                        let mut node_run = run.clone();
                        node_run.node_execution = Some(identity);
                        let context = WorkflowNodeContext { run: node_run, resume: resumes.remove(id), node_id: id.clone(), journal: journal.clone() };
                        let services = services.clone();
                        let graph = graph.clone();
                        let id = id.clone(); active.insert(id.clone());
                        tasks.spawn(async move {
                            let result = crate::WorkflowNodeNext { definition: &graph, middleware: &services.node_middleware, node: implementation.as_ref(), services: &services }.run(input, &context).await;
                            (id, result)
                        });
                    }
                }
                if tasks.is_empty() {
                    let latest = journal.state().await?;
                    // A skip can make a downstream node ready in this pass.
                    if !unresolved && !paused && self.order.iter().any(|id| latest.nodes[id].status == WorkflowNodeStatus::Pending && !matches!(self.readiness(id, &latest), Ready::Wait)) { continue; }
                    let (status, execution) = if unresolved { (RunStatus::RecoveryRequired, ExecutionStatus::RecoveryRequired) }
                        else if latest.nodes.values().any(|n| n.status == WorkflowNodeStatus::Suspended) {
                            let status = latest.nodes.values().filter_map(|n| n.result.as_ref())
                                .filter(|r| r.status != RunStatus::Completed)
                                .min_by_key(|r| match r.status { RunStatus::RecoveryRequired => 0, RunStatus::AwaitingInput => 1, _ => 2 }).unwrap().status.clone();
                            let execution = match status { RunStatus::RecoveryRequired => ExecutionStatus::RecoveryRequired,
                                RunStatus::StepLimit | RunStatus::ToolCallLimit => ExecutionStatus::Completed, _ => ExecutionStatus::Waiting };
                            (status, execution)
                        } else if latest.nodes.values().all(|n| matches!(n.status, WorkflowNodeStatus::Succeeded | WorkflowNodeStatus::Skipped)) {
                            (RunStatus::Completed, ExecutionStatus::Completed)
                        } else { return Err(AgentError::Run("DAG has no runnable nodes but is not settled".into())); };
                    let result = self.result(&latest, status, &run);
                    journal.change(|s, cp| {
                        cp.status = execution;
                        s.result = Some(result.clone());
                        if execution == ExecutionStatus::Completed { cp.transcript.push(Message::assistant(result.content.clone())); }
                        Ok(None)
                    }).await?;
                    return Ok(result);
                }
                let joined = tokio::select! {
                    biased;
                    _ = run.cancelled() => return Err(AgentError::Cancelled),
                    _ = tokio::time::sleep_until(run.deadline) => return Err(AgentError::Run("DAG run deadline exceeded".into())),
                    result = tasks.join_next() => result.expect("nonempty task set"),
                };
                let (id, result) = joined.map_err(|_| AgentError::Run("DAG task was aborted".into()))?;
                active.remove(&id);
                let result = result.and_then(|r| { self.definition.validate_result(&id, &r)?; Ok(r) });
                match result {
                    Ok(result) => {
                        let status = if result.status == RunStatus::Completed { WorkflowNodeStatus::Succeeded } else { WorkflowNodeStatus::Suspended };
                        journal.change(|s, _| { let n = s.nodes.get_mut(&id).unwrap(); n.status = status; n.result = Some(result); n.error = None; Ok(Some(node_event(&id, status))) }).await?;
                    }
                    Err(error) => {
                        tasks.abort_all();
                        while tasks.join_next().await.is_some() {}
                        if !error.contains_state_error() {
                            let saved = journal.change(|s, _| { let n = s.nodes.get_mut(&id).unwrap(); n.status = WorkflowNodeStatus::Failed; n.error = Some(error.to_string()); Ok(Some(node_event(&id, WorkflowNodeStatus::Failed))) }).await;
                            return finish_with_cleanup(Err(error), saved);
                        }
                        return Err(error);
                    }
                }
            }
        }.await;
        // JoinSet owns every node future: fail-fast, cancellation, panic and Drop
        // never detach siblings. Draining drops permits before final accounting.
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        if let Err(error) = &execution
            && !error.contains_state_error()
            && !journal.poisoned.load(Ordering::Acquire)
        {
            let saved = journal
                .change(|s, cp| {
                    let uncertain = s.nodes.values().any(|n| {
                        matches!(
                            n.status,
                            WorkflowNodeStatus::Running | WorkflowNodeStatus::Failed
                        )
                    });
                    cp.status = if uncertain {
                        ExecutionStatus::RecoveryRequired
                    } else if matches!(error.primary(), AgentError::Cancelled) {
                        ExecutionStatus::Cancelled
                    } else {
                        ExecutionStatus::Failed
                    };
                    Ok(None)
                })
                .await;
            return finish_with_cleanup(execution, saved);
        }
        execution
    }
    fn node(&self, id: &str) -> &DagNode {
        self.definition
            .nodes
            .iter()
            .find(|n| n.id == id)
            .expect("compiled node ID")
    }
    fn readiness(&self, id: &str, state: &DagWorkflowState) -> Ready {
        let incoming: Vec<_> = self
            .definition
            .edges
            .iter()
            .filter(|e| e.to == id)
            .collect();
        if incoming.is_empty() {
            return Ready::Run;
        }
        if incoming.iter().any(|e| {
            !matches!(
                state.nodes[&e.from].status,
                WorkflowNodeStatus::Succeeded | WorkflowNodeStatus::Skipped
            )
        }) {
            return Ready::Wait;
        }
        let count = incoming
            .iter()
            .filter(|e| selected(&state.nodes[&e.from], id))
            .count();
        if count == 0 || (self.node(id).join == WorkflowJoin::All && count != incoming.len()) {
            Ready::Skip
        } else {
            Ready::Run
        }
    }
    fn result(&self, state: &DagWorkflowState, status: RunStatus, run: &RunContext) -> RunResult {
        let outputs: BTreeMap<_, _> = state
            .nodes
            .iter()
            .filter(|(id, n)| {
                n.status == WorkflowNodeStatus::Succeeded
                    && !self
                        .definition
                        .edges
                        .iter()
                        .any(|e| e.from == **id && selected(n, &e.to))
            })
            .map(|(id, n)| (id.clone(), n.result.as_ref().unwrap().value.clone()))
            .collect();
        let value = if outputs.len() == 1 {
            outputs.values().next().unwrap().clone()
        } else {
            serde_json::json!(outputs)
        };
        let typed_content = if outputs.len() == 1 {
            state.nodes[outputs.keys().next().unwrap()]
                .result
                .as_ref()
                .and_then(|r| r.content.clone())
        } else {
            None
        };
        let content = if let Some(content) = typed_content {
            content
        } else if let Value::String(text) = &value {
            Content::from(text.clone())
        } else {
            Content(vec![ContentBlock::Json {
                value: value.clone(),
            }])
        };
        RunResult {
            output: content.text(),
            content,
            structured: Some(value),
            status,
            usage: run.usage(),
            interactions: state
                .nodes
                .values()
                .filter_map(|n| n.result.as_ref())
                .flat_map(|r| r.interactions.clone())
                .collect(),
            steps: state
                .nodes
                .values()
                .filter_map(|n| n.result.as_ref())
                .map(|r| r.steps)
                .sum(),
            tool_calls: state
                .nodes
                .values()
                .filter_map(|n| n.result.as_ref())
                .map(|r| r.tool_calls)
                .sum(),
        }
    }
}
enum Ready {
    Wait,
    Skip,
    Run,
}
fn selected(node: &WorkflowNodeState, to: &str) -> bool {
    node.status == WorkflowNodeStatus::Succeeded
        && node
            .result
            .as_ref()
            .is_some_and(|r| r.routes.as_ref().is_none_or(|routes| routes.contains(to)))
}
fn node_event(id: &str, status: WorkflowNodeStatus) -> AgentEvent {
    AgentEvent::WorkflowNodeChanged {
        node_id: id.to_owned(),
        status,
    }
}
fn reconcile_node<'a>(
    state: &'a mut DagWorkflowState,
    id: &str,
    invocation: RunId,
) -> Result<&'a mut WorkflowNodeState, AgentError> {
    let n = state
        .nodes
        .get_mut(id)
        .ok_or_else(|| invalid("unknown reconciliation node"))?;
    if n.invocation_id != invocation
        || !matches!(
            n.status,
            WorkflowNodeStatus::Running
                | WorkflowNodeStatus::Failed
                | WorkflowNodeStatus::Suspended
        )
    {
        return Err(invalid(
            "reconciliation requires the current interrupted node invocation",
        ));
    }
    Ok(n)
}
fn embedded_workflow(node: &WorkflowNodeState) -> Option<WorkflowState> {
    let cp: Checkpoint = serde_json::from_value(node.checkpoint.clone()?).ok()?;
    serde_json::from_value(cp.state).ok()
}

/// Built-in durable ReAct node. Its entire inner checkpoint is committed in the
/// parent's fenced transaction; it never opens a separate storage session.
pub struct ReactWorkflowNode;
#[async_trait]
impl WorkflowNode for ReactWorkflowNode {
    fn kind(&self) -> &str {
        "framework.react"
    }
    fn requires_model(&self) -> bool {
        true
    }
    fn supports_tool_policy(&self) -> bool {
        true
    }
    fn configuration_schema(&self) -> Value {
        serde_json::json!({"type":"object", "required":["name","instructions","generation","tools"],
        "additionalProperties":false, "properties": {
            "name":{"type":"string","minLength":1},
            "instructions":{"type":"array","items":{"type":"object"}},
            "generation":{"type":["object","null"]},
            "tools":{"type":["array","null"],"items":{"type":"string"},"uniqueItems":true}
        }})
    }
    fn supports_resume(&self) -> bool {
        true
    }
    fn validate(&self, config: &Value) -> Result<(), AgentError> {
        let stage: WorkflowStage = decode(config.clone())?;
        WorkflowDefinition {
            id: "react-node".into(),
            version: 1,
            stages: vec![stage],
        }
        .validate()
    }
    async fn execute(
        &self,
        input: WorkflowNodeInput,
        context: &WorkflowNodeContext,
        services: &AgentServices,
    ) -> Result<WorkflowNodeResult, AgentError> {
        let stage: WorkflowStage = decode(input.config)?;
        let definition = WorkflowDefinition {
            id: format!(
                "{}/{}",
                context.journal.state().await?.definition.id,
                input.node_id
            ),
            version: context.journal.state().await?.definition.version,
            stages: vec![stage],
        };
        let mut request = input.request;
        request.extensions = context.run.extensions.clone();
        request.resume = if input.checkpoint.is_some() {
            Some(context.resume.clone().unwrap_or_else(|| ResumeRequest {
                run_id: context.run.run_id,
                ..Default::default()
            }))
        } else {
            None
        };
        if !input.predecessors.is_empty() {
            request.input.0.push(ContentBlock::Text {
                text: "Workflow predecessor outputs:".into(),
            });
            request.input.0.push(ContentBlock::Json {
                value: serde_json::json!(input.predecessors),
            });
            for (id, content) in input.predecessor_content {
                request.input.0.push(ContentBlock::Text {
                    text: format!("\nNode {id} content:\n"),
                });
                request.input.0.extend(content.0);
            }
        }
        let mut services = services.clone();
        services.state = Some(Arc::new(EmbeddedWorkflowStore {
            context: context.clone(),
        }));
        let result = WorkflowLoop::new(definition)?
            .run(&request, &context.run, &services)
            .await?;
        Ok(WorkflowNodeResult {
            content: Some(result.content),
            value: result
                .structured
                .unwrap_or_else(|| Value::String(result.output)),
            routes: None,
            status: result.status,
            interactions: result.interactions,
            steps: result.steps,
            tool_calls: result.tool_calls,
        })
    }
}

struct EmbeddedWorkflowStore {
    context: WorkflowNodeContext,
}
impl EmbeddedWorkflowStore {
    fn check_lease(&self, lease: &ExecutionLease) -> Result<(), StateError> {
        let expected = &self.context.journal.lease;
        if lease.run_id != expected.run_id
            || lease.owner != expected.owner
            || lease.generation != expected.generation
        {
            return Err(StateError::LeaseLost);
        }
        if self.context.journal.poisoned.load(Ordering::Acquire) {
            return Err(StateError::Backend("parent journal is uncertain".into()));
        }
        Ok(())
    }
    fn state_error(error: AgentError) -> StateError {
        match error {
            AgentError::State(e) => e,
            _ => StateError::Invalid(error.to_string()),
        }
    }
}
#[async_trait]
impl StateStore for EmbeddedWorkflowStore {
    async fn create(
        &self,
        mut checkpoint: Checkpoint,
        previous: Option<RunId>,
    ) -> Result<Checkpoint, StateError> {
        if previous.is_some() {
            return Err(StateError::Conflict);
        }
        self.context
            .journal
            .change(|state, parent| {
                let n = state.nodes.get_mut(&self.context.node_id).unwrap();
                if n.checkpoint.is_some() {
                    return Err(StateError::Conflict.into());
                }
                if checkpoint.run_id != parent.run_id || checkpoint.session_id != parent.session_id
                {
                    return Err(invalid("embedded checkpoint identity mismatch"));
                }
                let mut history = parent.transcript.clone();
                history.pop(); // Replace the current user input with this node's augmented input.
                history.extend(checkpoint.transcript.clone());
                checkpoint.transcript = history;
                checkpoint.validate()?;
                n.checkpoint = Some(encode(&checkpoint)?);
                Ok(None)
            })
            .await
            .map_err(Self::state_error)?;
        Ok(checkpoint)
    }
    async fn load(&self, run_id: RunId) -> Result<Checkpoint, StateError> {
        if run_id != self.context.run.run_id {
            return Err(StateError::NotFound);
        }
        let state = self
            .context
            .journal
            .state()
            .await
            .map_err(Self::state_error)?;
        decode(
            state.nodes[&self.context.node_id]
                .checkpoint
                .clone()
                .ok_or(StateError::NotFound)?,
        )
        .map_err(Self::state_error)
    }
    async fn latest(&self, _: &str) -> Result<Option<Checkpoint>, StateError> {
        Ok(None)
    }
    async fn claim(
        &self,
        run_id: RunId,
        _: RunId,
        _: Duration,
    ) -> Result<ExecutionLease, StateError> {
        if run_id != self.context.run.run_id {
            return Err(StateError::NotFound);
        }
        let _current = self.context.journal.current.lock().await;
        let lease = self.context.journal.lease.clone();
        self.check_lease(&lease)?;
        Ok(lease)
    }
    async fn renew(
        &self,
        lease: &ExecutionLease,
        _: Duration,
    ) -> Result<ExecutionLease, StateError> {
        let _current = self.context.journal.current.lock().await;
        self.check_lease(lease)?;
        // Only the outer executor renews the authoritative storage lease.
        Ok(lease.clone())
    }
    async fn commit(
        &self,
        lease: &ExecutionLease,
        revision: u64,
        mut next: Checkpoint,
    ) -> Result<Checkpoint, StateError> {
        // The parent journal lock serializes all nested commits and budget writes.
        // A concurrent write-in-progress is not itself an uncertain outcome.
        if lease.run_id != self.context.journal.lease.run_id
            || lease.owner != self.context.journal.lease.owner
            || lease.generation != self.context.journal.lease.generation
        {
            return Err(StateError::LeaseLost);
        }
        self.context
            .journal
            .change(|state, _| {
                let n = state.nodes.get_mut(&self.context.node_id).unwrap();
                let current: Checkpoint =
                    decode(n.checkpoint.clone().ok_or(StateError::NotFound)?)?;
                if current.revision != revision {
                    return Err(StateError::Conflict.into());
                }
                current.validate_successor(&next)?;
                next.revision = current
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| invalid("embedded revision overflow"))?;
                n.checkpoint = Some(encode(&next)?);
                Ok(None)
            })
            .await
            .map_err(Self::state_error)?;
        Ok(next)
    }
    async fn release(&self, _: &ExecutionLease) -> Result<(), StateError> {
        Ok(())
    }
}
