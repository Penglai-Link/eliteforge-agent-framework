use crate::*;
use async_trait::async_trait;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

/// Delegates execution to an external runtime while retaining Framework's run
/// admission, middleware, lifecycle events, cancellation and timeout path.
pub struct ExternalAgentLoop;

struct ExternalEvents<'a> {
    context: &'a RunContext,
    message_id: String,
    text: Mutex<String>,
    reasoning: Mutex<String>,
    tools: Mutex<Vec<HistoryToolInvocation>>,
    finished: AtomicBool,
}

impl<'a> ExternalEvents<'a> {
    fn new(context: &'a RunContext) -> Self {
        let message_id = RunId::new_v4().to_string();
        context.emit_output(OutputEvent::MessageStarted {
            message_id: message_id.clone(),
        });
        Self {
            context,
            message_id,
            text: Mutex::new(String::new()),
            reasoning: Mutex::new(String::new()),
            tools: Mutex::new(Vec::new()),
            finished: AtomicBool::new(false),
        }
    }

    fn finish(&self, result: &ExternalAgentResult) {
        let streamed = self.text.lock().unwrap_or_else(|error| error.into_inner());
        let matches = result.content.text().starts_with(streamed.as_str());
        self.context.emit_output(OutputEvent::MessageFinished {
            message_id: self.message_id.clone(),
            content: if matches {
                result.content.clone()
            } else {
                streamed.clone().into()
            },
            structured: if matches {
                result.structured.clone()
            } else {
                None
            },
            tool_calls: Vec::new(),
        });
        self.finished.store(true, Ordering::Release);
    }
}

impl Drop for ExternalEvents<'_> {
    fn drop(&mut self) {
        if !self.finished.load(Ordering::Acquire) {
            self.context.emit_output(OutputEvent::MessageAborted {
                message_id: self.message_id.clone(),
            });
        }
    }
}

#[async_trait]
impl ExternalAgentEventSink for ExternalEvents<'_> {
    async fn emit(&self, event: ExternalAgentEvent) {
        match &event {
            ExternalAgentEvent::TextDelta(delta) => {
                {
                    let mut text = self.text.lock().unwrap_or_else(|error| error.into_inner());
                    text.push_str(delta);
                    self.context.emit_output(OutputEvent::TextDelta {
                        message_id: self.message_id.clone(),
                        delta: delta.clone(),
                    });
                }
                self.context.text_delta(delta).await;
            }
            ExternalAgentEvent::ReasoningDelta(delta) => {
                self.reasoning
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_str(delta);
                self.context.emit_output(OutputEvent::ReasoningDelta {
                    message_id: self.message_id.clone(),
                    delta: delta.clone(),
                });
                self.context.reasoning_delta(delta).await;
            }
            ExternalAgentEvent::Tool { name, detail } => {
                // Opaque runtimes may not provide a reliable tool result. Keep
                // that uncertainty rather than inventing a completed exchange.
                let id = RunId::new_v4();
                self.tools
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(HistoryToolInvocation {
                        id,
                        run_id: self.context.run_id,
                        step_id: self.message_id.clone(),
                        message_id: self.message_id.clone(),
                        call_id: detail
                            .get("callId")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_owned(),
                        tool_id: name.clone(),
                        tool_name: name.clone(),
                        arguments: detail
                            .get("arguments")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                        status: HistoryStatus::Unknown,
                        content: Vec::new(),
                        structured: Some(detail.clone()),
                        is_error: None,
                        result_reference: None,
                        started_at_ms: None,
                        finished_at_ms: None,
                    });
                self.context.emit_output(OutputEvent::Custom {
                    name: "framework.external_agent".into(),
                    value: serde_json::to_value(&event).expect("serializable observation"),
                });
            }
            _ => self.context.emit_output(OutputEvent::Custom {
                name: "framework.external_agent".into(),
                value: serde_json::to_value(&event).expect("external observation is serializable"),
            }),
        }
        self.context.emit(AgentEvent::ExternalAgent(event)).await;
    }
}

#[async_trait]
impl AgentLoop for ExternalAgentLoop {
    fn execution_kind(&self) -> &str {
        "EXTERNAL"
    }
    fn cooperative_cancellation(&self) -> bool {
        true
    }
    fn supports_steering(&self) -> bool {
        false
    }
    fn requires_conversations(&self) -> bool {
        false
    }
    fn validate_services(&self, services: &AgentServices) -> Result<(), AgentError> {
        services
            .external_agent
            .as_ref()
            .map(|_| ())
            .ok_or(AgentError::MissingService("ExternalAgent"))
    }
    async fn run(
        &self,
        request: &RunRequest,
        context: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let external = services
            .external_agent
            .as_ref()
            .ok_or(AgentError::MissingService("ExternalAgent"))?;
        let session = request
            .extensions
            .get::<ExternalSessionRef>()
            .map(|value| (*value).clone());
        context.check_cancelled()?;
        if let Some(budget) = context.extensions.get::<ContinuationBudget>()
            && !budget.admit_step(
                context.execution_counters.as_ref(),
                request.limits.max_steps,
            )
        {
            return Err(AgentError::Run("continuation step budget exhausted".into()));
        }
        if let Some(counter) = context.extensions.get::<ModelDispatchCounter>() {
            counter.record_step();
        }
        let (mut permit, _) = crate::budget::UsagePermit::reserve_child(
            context.usage.clone(),
            context.token_ceiling,
            context.token_ceiling,
        )?;
        context.persist_budget().await?;
        if let Some(budget) = context.extensions.get::<ContinuationBudget>()
            && let Err(error) = budget.admit_model_call()
        {
            permit.release_unspent();
            context.persist_budget().await?;
            return Err(error.into());
        }
        if let Some(counter) = context.extensions.get::<ModelDispatchCounter>() {
            counter.record_opaque();
        }
        let events = ExternalEvents::new(context);
        let mut durable = if let Some(store) = &services.state {
            let mut checkpoint = Checkpoint::new(
                context.run_id,
                &request.session_id,
                "external",
                1,
                serde_json::json!({"dispatched":true}),
            )
            .with_execution_scope(request.scope())
            .with_host_metadata(request);
            checkpoint.history.messages.push(
                HistoryMessage::user(context.run_id, &request.input)
                    .with_provenance(request.message_provenance.clone()),
            );
            checkpoint.history.steps.push(HistoryStep {
                id: events.message_id.clone(),
                run_id: context.run_id,
                node_invocation_id: None,
                sequence: 1,
                attempt: 1,
                name: "external".into(),
                model: request.model.clone(),
                status: HistoryStatus::Running,
                usage: None,
                started_at_ms: unix_time_ms(),
                finished_at_ms: None,
            });
            checkpoint.history.messages.push(HistoryMessage {
                requested_skills: Vec::new(),
                id: events.message_id.clone(),
                run_id: context.run_id,
                step_id: Some(events.message_id.clone()),
                role: Role::Assistant,
                provenance: context.assistant_provenance(),
                content: Vec::new(),
                structured: None,
                status: HistoryStatus::Running,
                created_at_ms: unix_time_ms(),
            });
            let previous = store
                .latest_scope(&request.session_id, request.scope())
                .await?;
            let checkpoint = store.create(checkpoint, previous.map(|p| p.run_id)).await?;
            let ttl = std::time::Duration::from_secs(30);
            let lease = store.claim(context.run_id, RunId::new_v4(), ttl).await?;
            let guard = crate::model_execution::WorkflowLease::new(
                store.clone(),
                lease.clone(),
                ttl,
                request.limits.cleanup_timeout,
                context.clone(),
            );
            Some((store.clone(), checkpoint, lease, guard))
        } else {
            None
        };
        let result = tokio::select! {
            biased;
            _=context.cancellation.cancelled()=>Err(if tokio::time::Instant::now() >= context.deadline {AgentError::Run("external run deadline exceeded".into())} else {AgentError::Cancelled}),
            _=tokio::time::sleep_until(context.deadline)=>Err(AgentError::Run("external run deadline exceeded".into())),
            result=external.run(
                ExternalAgentRequest {
                    input: request.input.clone(),
                    model: request
                        .model
                        .as_ref()
                        .map(|model| format!("{}/{}", model.connection, model.model)),
                    session,
                    extensions: request.extensions.clone(),
                },
                &events,
            )=>result,
        };
        if let Some((store, mut checkpoint, lease, guard)) = durable.take() {
            let message = &mut checkpoint.history.messages[1];
            let reasoning = events
                .reasoning
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if !reasoning.is_empty() {
                message
                    .content
                    .push(HistoryContentBlock::Reasoning { text: reasoning });
            }
            match &result {
                Ok(result) => {
                    message.content.extend(history_content(&result.content));
                    message.structured = result.structured.clone();
                    message.status = HistoryStatus::Completed;
                    checkpoint.status = ExecutionStatus::Completed;
                    checkpoint.history.steps[0].usage = Some((&result.usage).into());
                }
                Err(error) => {
                    message.content.push(HistoryContentBlock::Text {
                        text: events
                            .text
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .clone(),
                    });
                    message.status = HistoryStatus::Interrupted;
                    checkpoint.status = if matches!(error, AgentError::Cancelled) {
                        ExecutionStatus::Cancelled
                    } else {
                        ExecutionStatus::Failed
                    };
                }
            }
            checkpoint.history.steps[0].status = match checkpoint.status {
                ExecutionStatus::Completed => HistoryStatus::Completed,
                ExecutionStatus::Cancelled => HistoryStatus::Interrupted,
                _ => HistoryStatus::Failed,
            };
            checkpoint.history.steps[0].finished_at_ms = Some(unix_time_ms());
            checkpoint.history.tool_invocations = events
                .tools
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let saved = store.commit(&lease, checkpoint.revision, checkpoint).await;
            let released = guard.close().await;
            saved?;
            released?;
        }
        let result = result?;
        // Zero reported usage is unknown, so the whole reserved allowance
        // remains charged. Reported usage itself stays honest in RunResult.
        permit.settle(&result.usage, context.token_ceiling)?;
        context.persist_budget().await?;
        let output = result.content.text();
        if let Some(session) = &result.session {
            events
                .emit(ExternalAgentEvent::Session(session.clone()))
                .await;
        }
        events.finish(&result);
        Ok(RunResult {
            output,
            content: result.content,
            structured: result.structured,
            interactions: Vec::new(),
            status: RunStatus::Completed,
            usage: result.usage,
            steps: 1,
            tool_calls: 0,
            external_session: result.session,
        })
    }
}
