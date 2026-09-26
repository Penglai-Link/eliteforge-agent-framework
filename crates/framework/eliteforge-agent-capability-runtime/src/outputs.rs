use crate::*;
use async_trait::async_trait;
use eliteforge_agent_core::*;
use serde_json::{Value, json};
use std::{
    any::TypeId,
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
/// Stable contract IDs select independent streams; TypeId is used only by the host adapter.
#[derive(Default)]
pub struct RemoteOutputs(pub BTreeMap<String, RemoteOutputStream>);
pub struct RemoteOutputStream {
    receiver: mpsc::Receiver<Value>,
    failure: Arc<Mutex<Option<String>>>,
    cancellation: CancellationToken,
    _lease: UsageLease,
}
impl RemoteOutputStream {
    pub async fn next(&mut self) -> RuntimeResult<Option<Value>> {
        if let Some(error) = self
            .failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            return Err(RuntimeError::Rpc(error));
        }
        let value = self.receiver.recv().await;
        if let Some(error) = self
            .failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            return Err(RuntimeError::Rpc(error));
        }
        Ok(value)
    }
}
impl Drop for RemoteOutputStream {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}
struct OutputChannel {
    sender: mpsc::Sender<OutputEvent>,
    failure: Arc<Mutex<Option<String>>>,
    cancellation: CancellationToken,
}
#[derive(Default)]
struct OutputChannels {
    channels: Mutex<BTreeMap<String, Arc<OutputChannel>>>,
    workers: Mutex<BTreeMap<String, Arc<OutputWorker>>>,
}
pub struct RemoteOutputFactory(pub RemoteImplementation);
impl RunOutputFactory for RemoteOutputFactory {
    fn output_type(&self) -> TypeId {
        TypeId::of::<RemoteOutputs>()
    }
    fn aggregates(&self) -> bool {
        true
    }
    fn prepare(
        &self,
        _: &RunRequest,
        _: RunId,
        extensions: &mut Extensions,
        outputs: &mut RunOutputs,
    ) -> Result<(), AgentError> {
        if outputs.get_mut::<RemoteOutputs>().is_none() {
            outputs.insert(RemoteOutputs::default())?;
        }
        if extensions.get::<OutputChannels>().is_none() {
            extensions.insert(OutputChannels::default());
        }
        let channels = extensions
            .get::<OutputChannels>()
            .expect("output channel directory");
        let (sender, input) = mpsc::channel(64);
        let (output, receiver) = mpsc::channel(64);
        let failure = Arc::new(Mutex::new(None));
        let cancellation = CancellationToken::new();
        let channel = Arc::new(OutputChannel {
            sender,
            failure: failure.clone(),
            cancellation: cancellation.clone(),
        });
        if channels
            .channels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(self.0.declaration.id.clone(), channel.clone())
            .is_some()
        {
            return Err(AgentError::InvalidConfiguration(
                "duplicate remote output contract ID".into(),
            ));
        }
        outputs
            .get_mut::<RemoteOutputs>()
            .expect("remote outputs")
            .0
            .insert(
                self.0.declaration.id.clone(),
                RemoteOutputStream {
                    _lease: self
                        .0
                        .instance
                        .occupy("output_stream", &self.0.declaration.id)
                        .map_err(|e| AgentError::Run(e.to_string()))?,
                    receiver,
                    failure,
                    cancellation,
                },
            );
        // No work starts here. The lifecycle callback starts the owned worker.
        channels
            .workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                self.0.declaration.id.clone(),
                Arc::new(OutputWorker {
                    implementation: self.0.clone(),
                    input: Mutex::new(Some(input)),
                    output: Mutex::new(Some(output)),
                    channel,
                    task: Mutex::new(None),
                }),
            );
        Ok(())
    }
}
struct OutputWorker {
    implementation: RemoteImplementation,
    input: Mutex<Option<mpsc::Receiver<OutputEvent>>>,
    output: Mutex<Option<mpsc::Sender<Value>>>,
    channel: Arc<OutputChannel>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}
pub struct RemoteOutputSink(pub RemoteImplementation);
#[async_trait]
impl EventSink for RemoteOutputSink {
    fn on_published(&self, run: &RunContext, event: &EventEnvelope) {
        let AgentEvent::Output(event) = &event.event else {
            return;
        };
        let Some(channels) = run.extensions.get::<OutputChannels>() else {
            return;
        };
        let channel = channels
            .channels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&self.0.declaration.id)
            .cloned();
        let Some(channel) = channel else {
            return;
        };
        // Start the worker lazily on the first publication. This hook only hands
        // off to a local bounded queue, and never waits for process I/O.
        if let Some(worker) = channels
            .workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&self.0.declaration.id)
            .cloned()
        {
            worker.start(run);
        }
        if channel.sender.try_send((**event).clone()).is_err() {
            *channel.failure.lock().unwrap_or_else(|e| e.into_inner()) =
                Some("remote output queue overflow or consumer closed".into());
            channel.cancellation.cancel();
            run.cancellation.cancel();
        }
    }
    async fn on_event(&self, run: &RunContext, event: &AgentEvent) {
        if matches!(
            event,
            AgentEvent::RunFinished { .. } | AgentEvent::RunFailed(_) | AgentEvent::RunCancelled
        ) {
            let worker = run.extensions.get::<OutputChannels>().and_then(|channels| {
                channels
                    .workers
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&self.0.declaration.id)
                    .cloned()
            });
            if let Some(worker) = worker {
                if !matches!(event, AgentEvent::RunFinished { .. }) {
                    worker.channel.cancellation.cancel();
                }
                worker.finish().await;
            }
        }
    }
}
impl OutputWorker {
    fn start(&self, run: &RunContext) {
        let Some(mut input) = self.input.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return;
        };
        let Some(output) = self.output.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return;
        };
        let implementation = self.implementation.clone();
        let channel = self.channel.clone();
        let run_id = run.run_id;
        let cancellation = run.cancellation.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {biased;_ = channel.cancellation.cancelled()=>break,event=input.recv()=>{let Some(event)=event else{break;};let terminal=matches!(event,OutputEvent::RunResult(_));let result=implementation.invoke("output",json!({"run_id":run_id,"event":event})).await;match result{Ok(value)=>{if output.send(value).await.is_err(){break;}},Err(error)=>{*channel.failure.lock().unwrap_or_else(|e|e.into_inner())=Some(error.to_string());cancellation.cancel();break;}}if terminal{break;}}}
            }
        });
        *self.task.lock().unwrap_or_else(|e| e.into_inner()) = Some(task);
    }
    async fn finish(&self) {
        let task = self.task.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(mut task) = task {
            struct AbortTask(tokio::task::AbortHandle);
            impl Drop for AbortTask {
                fn drop(&mut self) {
                    self.0.abort();
                }
            }
            let _guard = AbortTask(task.abort_handle());
            if tokio::time::timeout(std::time::Duration::from_secs(1), &mut task)
                .await
                .is_err()
            {
                *self
                    .channel
                    .failure
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) =
                    Some("remote output drain timed out".into());
                task.abort();
            }
        }
    }
}
impl Drop for OutputWorker {
    fn drop(&mut self) {
        if let Some(task) = self
            .task
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            task.abort();
        }
    }
}
