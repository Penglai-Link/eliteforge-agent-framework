//! AG-UI presentation plugin. HTTP routing, authorization and durable replay
//! belong to the host; this crate supplies typed events and SSE frames.
mod adapter;
use adapter::Adapter;
use async_trait::async_trait;
use eliteforge_agent_core::*;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

pub use ag_ui;
pub use ag_ui::{Event, EventType, SseFormatter};

#[derive(Clone, Debug)]
pub struct AgUiOptions {
    /// Maximum queued source events per execution.
    pub capacity: usize,
    /// Maximum serialized source or protocol event size.
    pub max_event_bytes: usize,
    /// Bounds simultaneously open messages, tools, steps and activities.
    pub max_open_items: usize,
    pub include_reasoning: bool,
    /// Run errors are generic unless the host explicitly enables their text.
    pub include_error_details: bool,
}
impl Default for AgUiOptions {
    fn default() -> Self {
        Self {
            capacity: 256,
            max_event_bytes: 1024 * 1024,
            max_open_items: 1024,
            include_reasoning: true,
            include_error_details: false,
        }
    }
}

#[derive(Clone, Default)]
pub struct AgUiPlugin {
    options: AgUiOptions,
}
impl AgUiPlugin {
    pub fn new(options: AgUiOptions) -> Result<Self, AgentError> {
        if options.capacity == 0
            || options.capacity > 4096
            || options.max_event_bytes < 1024
            || options.max_event_bytes > 16 * 1024 * 1024
            || options.max_open_items == 0
            || options.max_open_items > 4096
        {
            return Err(AgentError::InvalidConfiguration(
                "invalid AG-UI stream limits".into(),
            ));
        }
        Ok(Self { options })
    }

    fn validate_start(&self, request: &RunRequest) -> Result<(), AgentError> {
        let invalid = || {
            AgentError::InvalidConfiguration("AG-UI session identity exceeds event limit".into())
        };
        // Bound allocation first; JSON escaping and the repeated session field
        // are accounted for by serializing the actual admission event below.
        if request.session_id.len() > self.options.max_event_bytes {
            return Err(invalid());
        }
        let run_id = request
            .resume
            .as_ref()
            .map_or(RunId::nil(), |resume| resume.run_id);
        let execution_id = RunId::nil();
        let envelope = EventEnvelope {
            scope: EventScope::Live { execution_id },
            schema_version: 1,
            sequence: 1,
            // Reserve the full timestamp width, including a clock digit rollover.
            timestamp_unix_ms: u64::MAX,
            run_id,
            session_id: request.session_id.clone(),
            node_execution: None,
            event: AgentEvent::RunStarted {
                run_id,
                session_id: request.session_id.clone(),
            },
        };
        if !fits(&envelope, self.options.max_event_bytes) {
            return Err(invalid());
        }
        let events = Adapter::new(self.options.clone(), execution_id)
            .push(envelope)
            .map_err(|_| invalid())?;
        if !events
            .iter()
            .all(|event| fits(event, self.options.max_event_bytes))
        {
            return Err(invalid());
        }
        Ok(())
    }
}

impl RunOutputFactory for AgUiPlugin {
    fn output_type(&self) -> std::any::TypeId {
        std::any::TypeId::of::<AgUiStream>()
    }
    fn prepare(
        &self,
        request: &RunRequest,
        execution_id: RunId,
        extensions: &mut Extensions,
        outputs: &mut RunOutputs,
    ) -> Result<(), AgentError> {
        self.validate_start(request)?;
        let (sender, receiver) = mpsc::channel(self.options.capacity);
        let channel = Arc::new(Channel {
            state: Mutex::new(ChannelState {
                sender: Some(sender),
                execution_id: Some(execution_id),
            }),
            failure: Arc::new(Mutex::new(None)),
            max_event_bytes: self.options.max_event_bytes,
        });
        outputs.insert(AgUiStream {
            adapter: Adapter::new(self.options.clone(), execution_id),
            receiver,
            failure: channel.failure.clone(),
            pending: VecDeque::new(),
            ended: false,
        })?;
        extensions.insert(channel);
        Ok(())
    }
}

#[async_trait]
impl Plugin for AgUiPlugin {
    fn id(&self) -> &str {
        "agent.agui"
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let output: Arc<dyn RunOutputFactory> = Arc::new(self.clone());
        let events: Arc<dyn EventSink> = Arc::new(self.clone());
        context.contribute(RunOutputService(output.clone()))?;
        context.contribute_extension(output)?;
        context.contribute(EventService(events.clone()))?;
        context.contribute_extension(events)
    }
}
#[async_trait]
impl EventSink for AgUiPlugin {
    fn on_published(&self, context: &RunContext, event: &EventEnvelope) {
        if let Some(channel) = context.extensions.get::<Arc<Channel>>() {
            channel.send(event);
        }
    }
    async fn on_event(&self, _: &RunContext, _: &AgentEvent) {}
}

struct Channel {
    max_event_bytes: usize,
    state: Mutex<ChannelState>,
    failure: Arc<Mutex<Option<&'static str>>>,
}
struct ChannelState {
    sender: Option<mpsc::Sender<EventEnvelope>>,
    execution_id: Option<RunId>,
}
impl Channel {
    fn send(&self, event: &EventEnvelope) {
        let EventScope::Live { execution_id } = event.scope else {
            return;
        };
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.execution_id.is_none() && matches!(event.event, AgentEvent::RunStarted { .. }) {
            state.execution_id = Some(execution_id);
        }
        // Child runs or a later recovery attempt cannot write into this stream.
        if state.execution_id != Some(execution_id) || state.sender.is_none() {
            return;
        }
        if !relevant(&event.event) {
            return;
        }
        if !fits(event, self.max_event_bytes) {
            *self.failure.lock().unwrap_or_else(|e| e.into_inner()) = Some("AGUI_EVENT_TOO_LARGE");
            state.sender.take();
            return;
        }
        match state
            .sender
            .as_ref()
            .expect("sender checked")
            .try_send(event.clone())
        {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                *self.failure.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some("AGUI_STREAM_OVERFLOW");
                state.sender.take();
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                state.sender.take();
            }
        }
        if terminal(&event.event) {
            state.sender.take();
        }
    }
}
fn terminal(event: &AgentEvent) -> bool {
    matches!(
        event,
        AgentEvent::RunFinished { .. } | AgentEvent::RunCancelled | AgentEvent::RunFailed(_)
    )
}
fn relevant(event: &AgentEvent) -> bool {
    matches!(
        event,
        AgentEvent::RunStarted { .. }
            | AgentEvent::Output(_)
            | AgentEvent::WorkflowNodeChanged { .. }
            | AgentEvent::Steered(_)
            | AgentEvent::RunFinished { .. }
            | AgentEvent::RunCancelled
            | AgentEvent::RunFailed(_)
    )
}

/// A single consumer with ordered, typed AG-UI events. A capacity, size or
/// source-order failure yields RUN_ERROR and terminates this presentation stream;
/// it does not retry tools or implicitly cancel the agent. Use Agent::wait
/// for the authoritative execution result.
pub struct AgUiStream {
    adapter: Adapter,
    receiver: mpsc::Receiver<EventEnvelope>,
    // The receiver must never own a sender: abrupt executor shutdown must close
    // the channel even if no terminal event was published.
    failure: Arc<Mutex<Option<&'static str>>>,
    pending: VecDeque<Event>,
    ended: bool,
}
impl AgUiStream {
    pub async fn recv(&mut self) -> Option<Event> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some(event);
            }
            if self.ended {
                return None;
            }
            match self.receiver.recv().await {
                Some(event) => {
                    // Commit state only after the complete output batch validates;
                    // a rejected event must not lose already-open message ends.
                    let mut candidate = self.adapter.clone();
                    match candidate.push(event) {
                        Ok(events) => {
                            if events
                                .iter()
                                .all(|event| fits(event, self.adapter.options.max_event_bytes))
                            {
                                self.pending.extend(events);
                                self.adapter = candidate;
                                self.ended = self.adapter.finished();
                            } else {
                                self.fail("AGUI_EVENT_TOO_LARGE");
                            }
                        }
                        Err(code) => self.fail(code),
                    }
                }
                None => {
                    let failure = *self.failure.lock().unwrap_or_else(|e| e.into_inner());
                    self.fail(failure.unwrap_or("AGUI_STREAM_INTERRUPTED"));
                }
            }
        }
    }
    /// Encode one event with the SDK's standard `data: {json}\n\n` framing.
    pub async fn recv_sse(&mut self) -> Result<Option<String>, ag_ui::Error> {
        self.recv()
            .await
            .map(|event| SseFormatter::new().encode_to_string(&event))
            .transpose()
    }
    fn fail(&mut self, code: &'static str) {
        self.receiver.close();
        let mut closing = self.adapter.fail(code);
        for event in &mut closing {
            if !fits(event, self.adapter.options.max_event_bytes) {
                // A step's END may be slightly larger than START. Keep the
                // pairing and size bound even at that boundary; metadata is optional.
                event.base_mut().metadata = None;
                event.base_mut().timestamp = None;
            }
        }
        self.pending.extend(closing);
        self.ended = true;
    }
}

// Count serialized bytes without allocating an additional unbounded JSON string.
fn fits(value: &impl serde::Serialize, limit: usize) -> bool {
    struct Bound(usize);
    impl std::io::Write for Bound {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("event size limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Bound(limit), value).is_ok()
}
