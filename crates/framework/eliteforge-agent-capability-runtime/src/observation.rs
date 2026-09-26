use crate::*;
use async_trait::async_trait;
use eliteforge_agent_core::*;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::mpsc;

/// Optional observation subscription. Execution only performs bounded local enqueue.
pub struct RemoteObservationSink {
    sender: mpsc::Sender<Value>,
    categories: Vec<String>,
    dropped: AtomicU64,
    worker: tokio::task::AbortHandle,
}
impl RemoteObservationSink {
    pub fn new(implementation: RemoteImplementation) -> Arc<Self> {
        let categories = implementation.declaration.metadata["subscriptions"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let (sender, mut receiver) = mpsc::channel::<Value>(128);
        let worker=tokio::spawn(async move {
            while let Some(params)=receiver.recv().await {
                let Ok(peer)=implementation.instance.peer().await else{break;};
                if peer.request("extension.call",json!({"implementation":implementation.declaration.id,"operation":"on_event","params":params}),std::time::Duration::from_secs(5)).await.is_err(){break;}
            }
        }).abort_handle();
        Arc::new(Self {
            sender,
            categories,
            dropped: AtomicU64::new(0),
            worker,
        })
    }
    pub fn dropped_events(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}
#[async_trait]
impl EventSink for RemoteObservationSink {
    fn on_published(&self, context: &RunContext, envelope: &EventEnvelope) {
        let Ok(value) = serde_json::to_value(&envelope.event) else {
            return;
        };
        let category = value
            .as_str()
            .or_else(|| {
                value
                    .as_object()
                    .and_then(|v| v.keys().next())
                    .map(String::as_str)
            })
            .unwrap_or("");
        if !self.categories.iter().any(|c| c == category) {
            return;
        }
        if self
            .sender
            .try_send(json!({"run_id":context.run_id,"event":value}))
            .is_err()
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
    async fn on_event(&self, _: &RunContext, _: &AgentEvent) {}
}
impl Drop for RemoteObservationSink {
    fn drop(&mut self) {
        self.worker.abort();
    }
}
