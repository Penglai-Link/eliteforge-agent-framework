//! Versioned DAG definitions and durable, plugin-extensible execution.
//!
//! Graph indices are private compilation details. Checkpoints, routes and
//! continuation always use the stable IDs supplied by the definition.
use crate::*;
use async_trait::async_trait;
use serde::Serialize;
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
            || cp.execution_scope != request.scope()
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
    fn execution_kind(&self) -> &str {
        "WORKFLOW"
    }
    fn requires_model(&self) -> bool {
        self.definition
            .nodes
            .iter()
            .any(|node| node.kind == "framework.react")
    }
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
                return Err(AgentError::MissingService("ChatModel"));
            }
        }
        Ok(())
    }
    async fn validate_services_async(&self, services: &AgentServices) -> Result<(), AgentError> {
        self.validate_services(services)?;
        for node in &self.definition.nodes {
            services
                .workflow_nodes
                .get(&node.kind)
                .ok_or_else(|| invalid("missing node"))?
                .validate_async(&node.config)
                .await?;
        }
        Ok(())
    }
    async fn run(
        &self,
        request: &RunRequest,
        context: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        self.validate_services_async(services).await?;
        let store = services
            .state
            .clone()
            .ok_or(AgentError::MissingService("StateStore"))?;
        let cp = if let Some(resume) = &request.resume {
            store.load(resume.run_id).await?
        } else {
            let previous = store
                .latest_scope(&request.session_id, request.scope())
                .await?;
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
            )
            .with_execution_scope(request.scope())
            .with_host_metadata(request);
            if let Some(previous) = &previous {
                cp.transcript = previous.transcript.clone();
            }
            cp.transcript.push(
                Message::user(&request.input).with_provenance(request.message_provenance.clone()),
            );
            cp.history.messages.push(
                HistoryMessage::user(context.run_id, &request.input)
                    .with_provenance(request.message_provenance.clone()),
            );
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
            run.model = state.request.model.clone();
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
            run.limits.max_steps = run.limits.max_steps.min(state.request.limits.max_steps);
            run.limits.max_total_tokens = run
                .limits
                .max_total_tokens
                .min(state.request.limits.max_total_tokens);
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
        r.response.as_ref().is_some_and(|response| {
            !state
                .nodes
                .values()
                .any(|n| embedded_workflow(n).is_some_and(|s| s.answers.contains(response)))
        })
    }) {
        return Err(invalid("terminal DAG cannot accept a new answer"));
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
                            });
                        }
                        Ok(_) | Err(StateError::NotFound) => {}
                        Err(error) => return Err(error.into()),
                    }
                }
            }
        }
        journal
            .change(|state, cp| {
                if let Some(response) = request.resume.as_ref().and_then(|r| r.response.as_ref()) {
                    let owners: Vec<_> = state
                        .nodes
                        .iter()
                        .filter(|(_, n)| {
                            n.result.as_ref().is_some_and(|r| {
                                r.interactions
                                    .iter()
                                    .any(|i| i.id == response.interaction_id)
                            }) || embedded_workflow(n).is_some_and(|s| {
                                s.interactions
                                    .iter()
                                    .any(|i| i.id == response.interaction_id)
                            })
                        })
                        .map(|(id, _)| id.clone())
                        .collect();
                    if owners.len() != 1 {
                        return Err(invalid("answer must identify one pending DAG interaction"));
                    }
                    resumes
                        .entry(owners[0].clone())
                        .or_insert_with(|| ResumeRequest {
                            run_id: run.run_id,
                            ..Default::default()
                        })
                        .response = Some(response.clone());
                }
                cp.status = ExecutionStatus::Running;
                Ok(None)
            })
            .await?;

        let graph = Arc::new(self.definition.clone());
        let mut tasks = tokio::task::JoinSet::new();
        let mut active = BTreeSet::<String>::new();
        let execution = async {
            loop {
                run.check_cancelled()?;
                if tokio::time::Instant::now() >= run.deadline { return Err(AgentError::Run("DAG run deadline exceeded".into())); }
                let state = journal.state().await?;
                if state.nodes.iter().any(|(id, n)| n.status == WorkflowNodeStatus::Failed ||
                    (n.status == WorkflowNodeStatus::Running && !active.contains(id) && !services.workflow_nodes.get(&self.node(id).kind).expect("validated kind").supports_resume())) {
                    return Err(AgentError::Run("DAG node execution was interrupted before its result was saved".into()));
                }
                let paused = state.nodes.iter().any(|(id, n)| n.status == WorkflowNodeStatus::Suspended && !resumes.contains_key(id));
                for id in &self.order {
                    if active.len() >= self.definition.max_parallel_nodes { break; }
                    let node = &state.nodes[id];
                    if active.contains(id) { continue; }
                    let start = match node.status {
                        WorkflowNodeStatus::Running => true,
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
                if tasks.is_empty() {
                    let latest = journal.state().await?;
                    // A skip can make a downstream node ready in this pass.
                    if !paused && self.order.iter().any(|id| latest.nodes[id].status == WorkflowNodeStatus::Pending && !matches!(self.readiness(id, &latest), Ready::Wait)) { continue; }
                    let (status, execution) = if latest.nodes.values().any(|n| n.status == WorkflowNodeStatus::Suspended) {
                            let status = latest.nodes.values().filter_map(|n| n.result.as_ref())
                                .filter(|r| r.status != RunStatus::Completed)
                                .min_by_key(|r| match r.status { RunStatus::AwaitingInput => 0, _ => 1 }).unwrap().status.clone();
                            let execution = match status { RunStatus::StepLimit | RunStatus::ToolCallLimit => ExecutionStatus::Completed, _ => ExecutionStatus::Waiting };
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
                .change(|_, cp| {
                    cp.status = if matches!(error.primary(), AgentError::Cancelled) {
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
            external_session: None,
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
        // Node checkpoints retain the original initiator; the current resume
        // may carry a separately attributed intervention.
        request.message_provenance = context.run.message_provenance();
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
            value: result.structured.unwrap_or(Value::String(result.output)),
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
                if checkpoint.run_id != parent.run_id
                    || checkpoint.session_id != parent.session_id
                    || checkpoint.execution_scope != parent.execution_scope
                {
                    return Err(invalid("embedded checkpoint identity mismatch"));
                }
                let mut history = parent.transcript.clone();
                history.pop(); // Replace the current user input with this node's augmented input.
                history.extend(checkpoint.transcript.clone());
                checkpoint.transcript = history;
                checkpoint.validate()?;
                n.checkpoint = Some(encode(&checkpoint)?);
                parent.history.merge(&checkpoint.history);
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
    async fn latest_scope(&self, _: &str, _: &str) -> Result<Option<Checkpoint>, StateError> {
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
            .change(|state, parent| {
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
                parent.history.merge(&next.history);
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
