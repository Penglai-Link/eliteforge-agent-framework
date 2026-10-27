use crate::{AgUiOptions, Event};
use eliteforge_agent_core::*;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Default)]
struct MessageState {
    text_bytes: usize,
    text_hash: Sha256,
    text_open: bool,
    reasoning_open: bool,
}
#[derive(Clone)]
pub(crate) struct Adapter {
    pub options: AgUiOptions,
    execution_id: RunId,
    session_id: String,
    run_id: String,
    sequence: u64,
    timestamp: i64,
    started: bool,
    ended: bool,
    last_text: Option<(usize, [u8; 32])>,
    messages: BTreeMap<String, MessageState>,
    calls: BTreeSet<RunId>,
    external_calls: BTreeMap<RunId, HistoryStatus>,
    steps: BTreeMap<String, String>,
    activities: BTreeMap<String, String>,
    subagents: BTreeSet<RunId>,
    result: Option<Box<RunResult>>,
}
impl Adapter {
    pub fn new(options: AgUiOptions, execution_id: RunId) -> Self {
        Self {
            options,
            execution_id,
            session_id: String::new(),
            run_id: String::new(),
            sequence: 0,
            timestamp: 0,
            started: false,
            ended: false,
            last_text: None,
            messages: BTreeMap::new(),
            calls: BTreeSet::new(),
            external_calls: BTreeMap::new(),
            steps: BTreeMap::new(),
            activities: BTreeMap::new(),
            subagents: BTreeSet::new(),
            result: None,
        }
    }
    pub fn finished(&self) -> bool {
        self.ended
    }
    pub fn push(&mut self, envelope: EventEnvelope) -> Result<Vec<Event>, &'static str> {
        if self.ended
            || envelope.scope
                != (EventScope::Live {
                    execution_id: self.execution_id,
                })
            || envelope.sequence <= self.sequence
        {
            return Err("AGUI_EVENT_ORDER");
        }
        self.sequence = envelope.sequence;
        self.timestamp = envelope.timestamp_unix_ms.min(i64::MAX as u64) as i64;
        let owner = envelope
            .node_execution
            .as_ref()
            .map_or_else(String::new, |node| node.invocation_id.to_string());
        let mut out = Vec::new();
        if let AgentEvent::RunStarted { run_id, session_id } = &envelope.event {
            if self.started {
                return Err("AGUI_EVENT_ORDER");
            }
            self.started = true;
            self.run_id = run_id.to_string();
            self.session_id = session_id.clone();
            out.push(Event::run_started(
                self.session_id.clone(),
                self.execution_id.to_string(),
            ));
        } else {
            if !self.started
                || self.run_id != envelope.run_id.to_string()
                || self.session_id != envelope.session_id
            {
                return Err("AGUI_EVENT_ORDER");
            }
            match envelope.event {
                AgentEvent::Output(event) => self.output(*event, &owner, &mut out)?,
                AgentEvent::Steered(text) => {
                    out.push(Event::custom("framework.steered", json!({"text":text})))
                }
                AgentEvent::WorkflowNodeChanged { node_id, status } => {
                    let step = format!("node/{node_id}");
                    if matches!(status, WorkflowNodeStatus::Running) {
                        if self.steps.insert(step.clone(), step.clone()).is_none() {
                            out.push(Event::step_started(step));
                        }
                    } else if let Some(name) = self.steps.remove(&step) {
                        out.push(Event::step_finished(name));
                    }
                    out.push(Event::custom(
                        "framework.node",
                        json!({"nodeId":node_id,"status":status}),
                    ));
                }
                AgentEvent::Collaboration(event) => self.collaboration(*event, &mut out)?,
                AgentEvent::RunFinished { status } => self.finish(status, &mut out)?,
                AgentEvent::RunCancelled => {
                    self.close_open(&mut out);
                    out.push(
                        ag_ui::RunErrorEvent::new("Agent run cancelled")
                            .with_code("AGENT_CANCELLED")
                            .into(),
                    );
                    self.ended = true;
                }
                AgentEvent::RunFailed(message) => {
                    self.close_open(&mut out);
                    let message = if self.options.include_error_details {
                        message
                    } else {
                        "Agent run failed".into()
                    };
                    out.push(
                        ag_ui::RunErrorEvent::new(message)
                            .with_code("AGENT_RUN_FAILED")
                            .into(),
                    );
                    self.ended = true;
                }
                _ => {}
            }
        }
        if self.messages.len()
            + self.calls.len()
            + self.steps.len()
            + self.activities.len()
            + self.subagents.len()
            > self.options.max_open_items
        {
            return Err("AGUI_OPEN_ITEM_LIMIT");
        }
        self.stamp(&mut out, envelope.node_execution.as_ref());
        Ok(out)
    }
    fn collaboration(
        &mut self,
        event: CollaborationEvent,
        out: &mut Vec<Event>,
    ) -> Result<(), &'static str> {
        match event {
            CollaborationEvent::ChildRunStarted {
                child_run_id,
                session_id: _,
                depth: _,
            } => {
                if child_run_id.is_nil() || !self.subagents.insert(child_run_id) {
                    return Err("AGUI_SUBAGENT_ORDER");
                }
                out.push(
                    ag_ui::SubagentStartedEvent::new(child_run_id.to_string(), "agent_spawn")
                        .with_description("Delegated child agent")
                        .into(),
                );
            }
            CollaborationEvent::ChildRunDetached {
                child_run_id,
                session_id: _,
                depth: _,
            } => {
                if child_run_id.is_nil() || !self.subagents.insert(child_run_id) {
                    return Err("AGUI_SUBAGENT_ORDER");
                }
                out.push(
                    ag_ui::SubagentStartedEvent::new(child_run_id.to_string(), "agent_spawn")
                        .with_description("Detached background child agent")
                        .into(),
                );
            }
            CollaborationEvent::ChildRunFinished {
                child_run_id,
                status,
                interactions,
            } => {
                if !self.subagents.remove(&child_run_id) {
                    return Err("AGUI_SUBAGENT_ORDER");
                }
                let id = child_run_id.to_string();
                let interrupt_ids = interactions
                    .iter()
                    .map(|interaction| interaction.id.to_string())
                    .collect::<Vec<_>>();
                if !interactions.is_empty() {
                    let mut event = ag_ui::CustomEvent::new(
                        "framework.subagent.interactions",
                        json!({"interactions":&interactions}),
                    );
                    event.subagent_run_id = Some(id.clone().into());
                    out.push(event.into());
                }
                match status {
                    RunStatus::Completed => out.push(
                        ag_ui::SubagentFinishedEvent::new(id)
                            .with_outcome(ag_ui::SubagentOutcome::Success)
                            .into(),
                    ),
                    RunStatus::AwaitingInput => out.push(
                        ag_ui::SubagentFinishedEvent::new(id)
                            .with_outcome(ag_ui::SubagentOutcome::suspended(interrupt_ids))
                            .into(),
                    ),
                    RunStatus::StepLimit | RunStatus::ToolCallLimit => out.push(
                        ag_ui::SubagentErrorEvent::new(id, "Child agent execution limit reached")
                            .with_code("AGENT_LIMIT_REACHED")
                            .into(),
                    ),
                }
            }
            CollaborationEvent::ChildRunFailed {
                child_run_id,
                cancelled,
                message,
            } => {
                if !self.subagents.remove(&child_run_id) {
                    return Err("AGUI_SUBAGENT_ORDER");
                }
                let message = if self.options.include_error_details {
                    message
                } else if cancelled {
                    "Child agent run cancelled".into()
                } else {
                    "Child agent run failed".into()
                };
                let code = if cancelled {
                    "AGENT_CANCELLED"
                } else {
                    "AGENT_RUN_FAILED"
                };
                out.push(
                    ag_ui::SubagentErrorEvent::new(child_run_id.to_string(), message)
                        .with_code(code)
                        .into(),
                );
            }
            CollaborationEvent::ThreadCreated { summary } => out.push(Event::custom(
                "framework.thread.created",
                json!({
                    "sessionId":summary.session_id,
                    "title":summary.title,
                    "status":summary.status,
                    "latestRunId":summary.latest_run_id,
                }),
            )),
            CollaborationEvent::ThreadRunStarted {
                session_id,
                run_id,
                cause,
                interaction_id,
            } => out.push(Event::custom(
                "framework.thread.run.started",
                json!({
                    "sessionId":session_id,
                    "runId":run_id,
                    "cause":cause,
                    "interactionId":interaction_id,
                }),
            )),
            CollaborationEvent::ThreadRunFinished {
                session_id,
                run_id,
                status,
                interactions,
                error,
            } => out.push(Event::custom(
                "framework.thread.run.finished",
                json!({
                    "sessionId":session_id,
                    "runId":run_id,
                    "status":status,
                    "interactions":interactions,
                    "error":error.map(|message| if self.options.include_error_details {
                        message
                    } else {
                        "Thread run failed".into()
                    }),
                }),
            )),
            CollaborationEvent::ThreadClosed { session_id } => out.push(Event::custom(
                "framework.thread.closed",
                json!({"sessionId":session_id}),
            )),
        }
        Ok(())
    }
    fn output(
        &mut self,
        event: OutputEvent,
        owner: &str,
        out: &mut Vec<Event>,
    ) -> Result<(), &'static str> {
        match event {
            OutputEvent::StepStarted { id, name } => {
                if id.is_empty() || name.is_empty() {
                    return Err("AGUI_STEP_ORDER");
                }
                let key = format!("step/{owner}/{id}");
                let name = format!("{name} [{key}]");
                if self.steps.insert(key, name.clone()).is_some() {
                    return Err("AGUI_STEP_ORDER");
                }
                out.push(Event::step_started(name));
            }
            OutputEvent::StepFinished { id } => {
                let name = self
                    .steps
                    .remove(&format!("step/{owner}/{id}"))
                    .ok_or("AGUI_STEP_ORDER")?;
                out.push(Event::step_finished(name));
            }
            OutputEvent::MessageStarted { message_id } => {
                if message_id.is_empty()
                    || self
                        .messages
                        .insert(message_id, MessageState::default())
                        .is_some()
                {
                    return Err("AGUI_MESSAGE_ORDER");
                }
            }
            OutputEvent::TextDelta { message_id, delta } => {
                let state = self
                    .messages
                    .get_mut(&message_id)
                    .ok_or("AGUI_MESSAGE_ORDER")?;
                if !delta.is_empty() {
                    if state.text_bytes.saturating_add(delta.len()) > self.options.max_event_bytes {
                        return Err("AGUI_MESSAGE_TOO_LARGE");
                    }
                    open_text(&message_id, state, out);
                    state.text_bytes += delta.len();
                    state.text_hash.update(delta.as_bytes());
                    out.push(Event::text_message_content(message_id, delta));
                }
            }
            OutputEvent::ReasoningDelta { message_id, delta } => {
                let state = self
                    .messages
                    .get_mut(&message_id)
                    .ok_or("AGUI_MESSAGE_ORDER")?;
                if self.options.include_reasoning && !delta.is_empty() {
                    let id = format!("{message_id}/reasoning");
                    if !state.reasoning_open {
                        out.push(Event::reasoning_start(id.clone()));
                        out.push(Event::reasoning_message_start(id.clone()));
                        state.reasoning_open = true;
                    }
                    out.push(Event::reasoning_message_content(id, delta));
                }
            }
            OutputEvent::MessageFinished {
                message_id,
                content,
                structured,
                tool_calls,
            } => {
                let mut state = self
                    .messages
                    .remove(&message_id)
                    .ok_or("AGUI_MESSAGE_ORDER")?;
                let text = content.text();
                let prefix = text.get(..state.text_bytes).ok_or("AGUI_TEXT_MISMATCH")?;
                if Sha256::digest(prefix.as_bytes()) != state.text_hash.clone().finalize() {
                    return Err("AGUI_TEXT_MISMATCH");
                }
                if !text.is_empty() || !tool_calls.is_empty() {
                    open_text(&message_id, &mut state, out);
                }
                let remaining = &text[state.text_bytes..];
                if !remaining.is_empty() {
                    out.push(Event::text_message_content(message_id.clone(), remaining));
                }
                if !text.is_empty() {
                    self.last_text = Some(text_fingerprint(&text));
                }
                // TOOL_CALL_END closes the argument stream, before execution/results.
                for call in tool_calls {
                    if !self.calls.insert(call.invocation_id) {
                        return Err("AGUI_TOOL_ID_COLLISION");
                    }
                    let id = tool_id(call.invocation_id)?;
                    self.declare_tool(&id, &call, Some(&message_id), out)?;
                }
                if structured.is_some()
                    || content
                        .0
                        .iter()
                        .any(|part| !matches!(part, ContentBlock::Text { .. }))
                {
                    out.push(Event::custom(
                        "framework.message",
                        json!({"messageId":message_id,"content":content,"structured":structured}),
                    ));
                }
                close_message(&message_id, &state, out);
            }
            OutputEvent::MessageAborted { message_id } => {
                if let Some(state) = self.messages.remove(&message_id) {
                    close_message(&message_id, &state, out);
                }
            }
            OutputEvent::ToolResult { call, output } => {
                // A resumed result answers the original proposal, possibly in
                // an earlier stream. Never mint another call or repeat its args.
                let id = tool_id(call.invocation_id)?;
                self.calls.remove(&call.invocation_id);
                let payload = json!({"content":output.content,"structured":output.structured,
                    "isError":output.is_error,"effect":output.effect,"resultReference":output.result_reference});
                let content = serde_json::to_string(&payload).map_err(|_| "AGUI_SERIALIZATION")?;
                out.push(Event::tool_call_result(format!("{id}/result"), id, content));
            }
            OutputEvent::RunResult(result) => self.result = Some(result),
            OutputEvent::StateSnapshot(snapshot) => out.push(Event::state_snapshot(snapshot)),
            OutputEvent::StateDelta(delta) => {
                let patch: Vec<ag_ui::PatchOperation> =
                    serde_json::from_value(delta).map_err(|_| "AGUI_INVALID_PATCH")?;
                out.push(Event::state_delta(patch));
            }
            OutputEvent::ActivitySnapshot {
                id,
                activity_type,
                content,
            } => {
                let id = format!("{}/{owner}/activity/{id}", self.execution_id);
                self.activities.insert(id.clone(), activity_type.clone());
                out.push(Event::activity_snapshot(id, activity_type, content));
            }
            OutputEvent::ActivityDelta {
                id,
                activity_type,
                patch,
            } => {
                let id = format!("{}/{owner}/activity/{id}", self.execution_id);
                if self.activities.get(&id) != Some(&activity_type) {
                    return Err("AGUI_ACTIVITY_ORDER");
                }
                let patch: Vec<ag_ui::PatchOperation> =
                    serde_json::from_value(patch).map_err(|_| "AGUI_INVALID_PATCH")?;
                out.push(Event::activity_delta(id, activity_type, patch));
            }
            OutputEvent::Custom { name, value } => {
                if name == "framework.external_agent" {
                    self.external_tool(&value, out)?;
                }
                out.push(Event::custom(name, value));
            }
        }
        Ok(())
    }
    fn external_tool(
        &mut self,
        value: &serde_json::Value,
        out: &mut Vec<Event>,
    ) -> Result<(), &'static str> {
        let Some(tool) = value.get("Tool") else {
            return Ok(());
        };
        let detail = &tool["detail"];
        let Some(native_id) = detail
            .get("callId")
            .or_else(|| detail.get("id"))
            .and_then(|id| id.as_str())
            .filter(|id| !id.is_empty())
        else {
            return Ok(());
        };
        let Some(id) = detail
            .get("invocationId")
            .and_then(|id| id.as_str())
            .and_then(|id| RunId::parse_str(id).ok())
        else {
            return Ok(());
        };
        let status: HistoryStatus = serde_json::from_value(detail["historyStatus"].clone())
            .unwrap_or(HistoryStatus::Unknown);
        if status == HistoryStatus::Unknown {
            return Ok(());
        }
        let name = tool["name"].as_str().unwrap_or("external");
        if !self.external_calls.contains_key(&id) {
            let call = ToolCall {
                invocation_id: id,
                id: native_id.to_owned(),
                call_name: name.to_owned(),
                arguments: detail.get("arguments").cloned().unwrap_or(json!({})),
            };
            self.declare_tool(&tool_id(id)?, &call, detail["messageId"].as_str(), out)?;
        }
        let terminal = |status| {
            matches!(
                status,
                HistoryStatus::Completed | HistoryStatus::Failed | HistoryStatus::Interrupted
            )
        };
        let previous = self.external_calls.get(&id).copied();
        if matches!(status, HistoryStatus::Completed | HistoryStatus::Failed)
            && !previous.is_some_and(terminal)
        {
            let payload =
                json!({"content":[],"structured":detail,"isError":status == HistoryStatus::Failed});
            let id = tool_id(id)?;
            out.push(Event::tool_call_result(
                format!("{id}/result"),
                id,
                payload.to_string(),
            ));
        }
        if !previous.is_some_and(terminal) {
            self.external_calls.insert(id, status);
        }
        Ok(())
    }
    fn declare_tool(
        &self,
        id: &str,
        call: &ToolCall,
        parent: Option<&str>,
        out: &mut Vec<Event>,
    ) -> Result<(), &'static str> {
        if call.id.is_empty() || call.call_name.is_empty() {
            return Err("AGUI_INVALID_TOOL");
        }
        // Gateway envelopes present the inner tool, not the wrapper: the
        // human sees the real tool name and its actual arguments.
        let presented = presented_call(call);
        let call = presented.as_ref().unwrap_or(call);
        let mut start = ag_ui::ToolCallStartEvent::new(id, call.call_name.clone());
        start.parent_message_id = parent.map(Into::into);
        out.push(start.into());
        out.push(Event::tool_call_args(
            id,
            serde_json::to_string(&call.arguments).map_err(|_| "AGUI_SERIALIZATION")?,
        ));
        out.push(Event::tool_call_end(id));
        Ok(())
    }
    fn finish(&mut self, status: RunStatus, out: &mut Vec<Event>) -> Result<(), &'static str> {
        self.close_open(out);
        if !self.subagents.is_empty() {
            return Err("AGUI_SUBAGENT_ORDER");
        }
        if let Some(result) = &self.result {
            if result.status != status {
                return Err("AGUI_RESULT_MISMATCH");
            }
            if !result.output.is_empty() && self.last_text != Some(text_fingerprint(&result.output))
            {
                let id = format!("{}/result", self.execution_id);
                out.push(Event::text_message_start(
                    id.clone(),
                    ag_ui::TextMessageRole::Assistant,
                ));
                out.push(Event::text_message_content(
                    id.clone(),
                    result.output.clone(),
                ));
                out.push(Event::text_message_end(id));
            }
        }
        let outcome = match status {
            RunStatus::Completed => ag_ui::RunOutcome::Success,
            RunStatus::AwaitingInput => {
                let mut interrupts = Vec::new();
                if let Some(result) = &self.result {
                    for request in &result.interactions {
                        let reason = match request.kind {
                            InteractionKind::Approval { .. } => "tool_call",
                            InteractionKind::Input => "input_required",
                        };
                        let mut interrupt = ag_ui::Interrupt::new(request.id.to_string(), reason);
                        if let InteractionKind::Approval { invocation_id } = request.kind {
                            let details = request.presentation.clone().and_then(|value| {
                                serde_json::from_value::<ToolApprovalDetails>(value).ok()
                            });
                            let visible_id = details
                                .as_ref()
                                .and_then(|d| d.batch.as_ref())
                                .map_or(invocation_id, |batch| batch.parent_invocation_id);
                            interrupt.tool_call_id = Some(tool_id(visible_id)?.into());
                        }
                        interrupt.message = Some(request.prompt.text());
                        interrupt.response_schema = request.response_schema.as_object().cloned();
                        interrupt.metadata =
                            Some(json!({"interaction":request}).as_object().unwrap().clone());
                        interrupts.push(interrupt);
                    }
                }
                if interrupts.is_empty() {
                    interrupts.push(ag_ui::Interrupt::new(
                        format!("{}/input", self.execution_id),
                        "input_required",
                    ));
                }
                ag_ui::RunOutcome::interrupt(interrupts)
            }
            RunStatus::StepLimit | RunStatus::ToolCallLimit => {
                out.push(
                    ag_ui::RunErrorEvent::new("Agent execution limit reached")
                        .with_code("AGENT_LIMIT_REACHED")
                        .into(),
                );
                self.ended = true;
                return Ok(());
            }
        };
        let mut finished =
            ag_ui::RunFinishedEvent::new(self.session_id.clone(), self.execution_id.to_string())
                .with_outcome(outcome);
        if let Some(result) = self.result.take() {
            finished.result = Some(serde_json::to_value(result).map_err(|_| "AGUI_SERIALIZATION")?);
        }
        out.push(finished.into());
        self.ended = true;
        Ok(())
    }
    fn close_open(&mut self, out: &mut Vec<Event>) {
        for (id, state) in std::mem::take(&mut self.messages) {
            close_message(&id, &state, out);
        }
        for (_, step) in std::mem::take(&mut self.steps) {
            out.push(Event::step_finished(step));
        }
        self.calls.clear();
        self.activities.clear();
    }
    pub fn fail(&mut self, code: &'static str) -> Vec<Event> {
        let mut out = Vec::new();
        // Start is always queued first; a channel limit still reports an explicit
        // terminal error after all previously delivered start/end pairs close.
        self.close_open(&mut out);
        out.push(
            ag_ui::RunErrorEvent::new("AG-UI output is incomplete")
                .with_code(code)
                .into(),
        );
        self.ended = true;
        self.stamp(&mut out, None);
        out
    }
    fn stamp(&self, events: &mut [Event], node: Option<&WorkflowNodeExecution>) {
        for event in events {
            event.base_mut().timestamp = Some(self.timestamp);
            event.base_mut().metadata = Some(json!({"framework": {
                "runId":self.run_id,"sessionId":self.session_id,"executionId":self.execution_id,"sourceSequence":self.sequence,"node":node
            }}).as_object().unwrap().clone());
        }
    }
}
fn text_fingerprint(text: &str) -> (usize, [u8; 32]) {
    (text.len(), Sha256::digest(text.as_bytes()).into())
}
fn tool_id(invocation_id: RunId) -> Result<String, &'static str> {
    if invocation_id.is_nil() {
        return Err("AGUI_INVALID_TOOL");
    }
    Ok(invocation_id.to_string())
}
fn open_text(id: &str, state: &mut MessageState, out: &mut Vec<Event>) {
    if !state.text_open {
        out.push(Event::text_message_start(
            id,
            ag_ui::TextMessageRole::Assistant,
        ));
        state.text_open = true;
    }
}
fn close_message(id: &str, state: &MessageState, out: &mut Vec<Event>) {
    if state.reasoning_open {
        let id = format!("{id}/reasoning");
        out.push(Event::reasoning_message_end(id.clone()));
        out.push(Event::reasoning_end(id));
    }
    if state.text_open {
        out.push(Event::text_message_end(id));
    }
}
