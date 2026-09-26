//! Versioned DAG definitions and durable, plugin-extensible execution.
//!
//! Graph indices are private compilation details. Checkpoints, routes and
//! continuation always use the stable IDs supplied by the definition.
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
    #[doc(hidden)]
    pub fn compile(&self) -> Result<Vec<String>, AgentError> {
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
    #[doc(hidden)]
    pub fn validate_result(&self, id: &str, result: &WorkflowNodeResult) -> Result<(), AgentError> {
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
    /// Stable for this logical node invocation, including continuation.
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
#[crate::extension_point(adapter = crate::WorkflowNodePlugin, contribute)]
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
    async fn validate_async(&self, config: &Value) -> Result<(), AgentError> {
        self.validate(config)
    }
    fn requires_model(&self) -> bool {
        false
    }
    /// Declare this only for nodes that do not execute tools, or enforce every
    /// installed tool policy before dispatch (including resume).
    fn supports_tool_policy(&self) -> bool {
        false
    }
    /// True only when execute can safely continue from its persisted checkpoint.
    /// Interrupted nodes without this support fail the run.
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
    /// Stable across continuation.
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
    #[doc(hidden)]
    pub node_id: String,
    #[doc(hidden)]
    pub journal: Arc<DagJournal>,
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

#[doc(hidden)]
pub struct DagJournal {
    #[doc(hidden)]
    pub store: Arc<dyn StateStore>,
    #[doc(hidden)]
    pub lease: ExecutionLease,
    #[doc(hidden)]
    pub current: Mutex<Checkpoint>,
    #[doc(hidden)]
    pub poisoned: AtomicBool,
    #[doc(hidden)]
    pub context: RunContext,
}
impl DagJournal {
    #[doc(hidden)]
    pub async fn state(&self) -> Result<DagWorkflowState, AgentError> {
        decode(self.current.lock().await.state.clone())
    }
    #[doc(hidden)]
    pub async fn change<F>(&self, change: F) -> Result<(), AgentError>
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

fn node_event(id: &str, status: WorkflowNodeStatus) -> AgentEvent {
    AgentEvent::WorkflowNodeChanged {
        node_id: id.into(),
        status,
    }
}
