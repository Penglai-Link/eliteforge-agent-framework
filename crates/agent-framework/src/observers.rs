use crate::*;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::{Mutex, mpsc};

/// Bounded best-effort observation. Durable state never uses this queue.
pub(crate) struct ObserverDispatcher {
    sender: std::sync::Mutex<Option<mpsc::Sender<AgentEvent>>>,
    worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
    dropped: AtomicU64,
    terminal: std::sync::Mutex<Option<AgentEvent>>,
    context: RunContext,
}

impl ObserverDispatcher {
    pub fn start(mut context: RunContext, capacity: usize) -> Arc<Self> {
        context.observers = None;
        let (sender, mut receiver) = mpsc::channel(capacity);
        let observation_context = context.clone();
        let worker = tokio::spawn(async move {
            while let Some(event) = receiver.recv().await {
                for sink in &context.sinks {
                    notify_sink(
                        sink.as_ref(),
                        &context,
                        &event,
                        std::time::Duration::from_secs(1),
                    )
                    .await;
                }
            }
        });
        Arc::new(Self {
            sender: std::sync::Mutex::new(Some(sender)),
            worker: Mutex::new(Some(worker)),
            dropped: AtomicU64::new(0),
            terminal: std::sync::Mutex::new(None),
            context: observation_context,
        })
    }

    pub fn enqueue(&self, event: AgentEvent) {
        if matches!(
            event,
            AgentEvent::RunFinished { .. } | AgentEvent::RunFailed(_) | AgentEvent::RunCancelled
        ) {
            *self.terminal.lock().unwrap_or_else(|e| e.into_inner()) = Some(event);
            return;
        }
        if let Some(sender) = self
            .sender
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            && sender.try_send(event).is_err()
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub async fn finish(&self, timeout: std::time::Duration) {
        self.sender.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(mut worker) = self.worker.lock().await.take() {
            let _guard = AbortWorker(worker.abort_handle());
            if tokio::time::timeout(timeout, &mut worker).await.is_err() {
                worker.abort();
                let _ = worker.await;
            }
            // Stop ordinary callbacks before terminal delivery, preserving order
            // within each sink. Each terminal callback gets its own bounded
            // opportunity, independent of backlog and other sinks' responsiveness.
            let terminal = self
                .terminal
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if let Some(event) = terminal {
                let timeout = timeout.min(std::time::Duration::from_secs(1));
                futures::future::join_all(
                    self.context
                        .sinks
                        .iter()
                        .map(|sink| notify_sink(sink.as_ref(), &self.context, &event, timeout)),
                )
                .await;
            }
        }
    }
}

async fn notify_sink(
    sink: &dyn EventSink,
    context: &RunContext,
    event: &AgentEvent,
    timeout: std::time::Duration,
) {
    use futures::FutureExt;
    let callback =
        std::panic::AssertUnwindSafe(async { sink.on_event(context, event).await }).catch_unwind();
    let _ = tokio::time::timeout(timeout, callback).await;
}

struct AbortWorker(tokio::task::AbortHandle);
impl Drop for AbortWorker {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Drop for ObserverDispatcher {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.get_mut().take() {
            worker.abort();
        }
    }
}
