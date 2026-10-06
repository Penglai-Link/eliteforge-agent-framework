use super::*;
use async_trait::async_trait;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Clone, Copy)]
enum AdmissionFailure {
    Write,
    InterruptedWrite,
    Closed,
    RollbackWrite,
}
struct AdmissionJournal {
    mode: AdmissionFailure,
    snapshots: Mutex<Vec<(Usage, usize)>>,
    entered: tokio::sync::Notify,
}
#[async_trait]
impl crate::budget::BudgetJournal for AdmissionJournal {
    async fn save(
        &self,
        ledger: Arc<Mutex<crate::budget::UsageLedger>>,
        child_runs: Arc<AtomicUsize>,
    ) -> Result<(), StateError> {
        let attempt = {
            let mut snapshots = self.snapshots.lock().unwrap();
            snapshots.push((
                ledger.lock().unwrap().including_reservations(),
                child_runs.load(Ordering::Acquire),
            ));
            snapshots.len()
        };
        match (self.mode, attempt) {
            (AdmissionFailure::Write, 1) | (AdmissionFailure::RollbackWrite, 2) => {
                Err(StateError::Backend("write outcome unknown".into()))
            }
            (AdmissionFailure::InterruptedWrite, 1) => {
                self.entered.notify_one();
                std::future::pending().await
            }
            _ => Ok(()),
        }
    }
}
struct CountedChildLoop(Arc<AtomicUsize>);
fn admission_result(context: &RunContext) -> RunResult {
    RunResult {
        output: "done".into(),
        content: "done".into(),
        structured: None,
        interactions: vec![],
        status: RunStatus::Completed,
        usage: context.usage(),
        steps: 0,
        tool_calls: 0,
        external_session: None,
    }
}
#[async_trait]
impl AgentLoop for CountedChildLoop {
    async fn run(
        &self,
        _: &RunRequest,
        context: &RunContext,
        _: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(admission_result(context))
    }
}
struct AdmissionProbe {
    child: Agent,
    closed: Agent,
    dispatched: Arc<AtomicUsize>,
    journal: Arc<AdmissionJournal>,
}
#[async_trait]
impl AgentLoop for AdmissionProbe {
    async fn run(
        &self,
        _: &RunRequest,
        parent: &RunContext,
        _: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let mut context = parent.clone();
        context.budget_journal = Some(self.journal.clone());
        let mode = self.journal.mode;
        if matches!(mode, AdmissionFailure::InterruptedWrite) {
            let pending = self
                .child
                .run_child(RunRequest::new("interrupted", "input"), &context);
            tokio::pin!(pending);
            tokio::select! { biased; _ = &mut pending => panic!("journal should remain pending"), _ = self.journal.entered.notified() => {} }
            // The future and its admission guard drop at the end of this block.
        } else {
            let child = if matches!(mode, AdmissionFailure::Write) {
                &self.child
            } else {
                &self.closed
            };
            let error = child
                .run_child(RunRequest::new("rejected", "input"), &context)
                .await
                .unwrap_err();
            match mode {
                AdmissionFailure::Write => assert!(matches!(error, AgentError::State(_))),
                AdmissionFailure::Closed => assert!(matches!(error, AgentError::Closed)),
                AdmissionFailure::RollbackWrite => {
                    assert!(matches!(error.primary(), AgentError::Closed));
                    assert!(
                        matches!(error, AgentError::WithCleanup { cleanup, .. } if matches!(*cleanup, AgentError::State(_)))
                    );
                }
                AdmissionFailure::InterruptedWrite => unreachable!(),
            }
        }
        assert_eq!(self.dispatched.load(Ordering::SeqCst), 0);
        assert_eq!(context.usage(), Usage::default());
        assert_eq!(context.child_runs.load(Ordering::Acquire), 0);
        {
            let snapshots = self.journal.snapshots.lock().unwrap();
            assert_eq!(
                snapshots[0].0.input_tokens + snapshots[0].0.output_tokens,
                100
            );
            assert_eq!(snapshots[0].1, 1);
            if matches!(
                mode,
                AdmissionFailure::Write | AdmissionFailure::InterruptedWrite
            ) {
                assert_eq!(
                    snapshots.len(),
                    1,
                    "do not guess another write after an unknown outcome"
                );
            } else {
                assert_eq!(
                    snapshots.as_slice(),
                    &[(snapshots[0].0.clone(), 1), (Usage::default(), 0)]
                );
            }
        }
        context.budget_journal = None;
        self.child
            .run_child(RunRequest::new("valid", "input"), &context)
            .await?;
        assert_eq!(self.dispatched.load(Ordering::SeqCst), 1);
        Ok(admission_result(&context))
    }
}
#[tokio::test]
async fn failed_and_interrupted_child_admission_refunds_locally_and_respects_unknown_writes() {
    for mode in [
        AdmissionFailure::Write,
        AdmissionFailure::InterruptedWrite,
        AdmissionFailure::Closed,
        AdmissionFailure::RollbackWrite,
    ] {
        let dispatched = Arc::new(AtomicUsize::new(0));
        let child = FrameworkBuilder::new()
            .defaults(AgentSelection {
                agent_loop: Some("framework.loop".into()),
                ..AgentSelection::default()
            })
            .plugin(ConversationStorePlugin::new(UnusedConversations))
            .plugin(AgentLoopPlugin::new(CountedChildLoop(dispatched.clone())))
            .build()
            .await
            .unwrap()
            .agent(AgentSelection::default())
            .build()
            .await
            .unwrap();
        let closed = FrameworkBuilder::new()
            .defaults(AgentSelection {
                agent_loop: Some("framework.loop".into()),
                ..AgentSelection::default()
            })
            .plugin(ConversationStorePlugin::new(UnusedConversations))
            .plugin(AgentLoopPlugin::new(CountedChildLoop(dispatched.clone())))
            .build()
            .await
            .unwrap()
            .agent(AgentSelection::default())
            .build()
            .await
            .unwrap();
        closed.close().await;
        let parent = FrameworkBuilder::new()
            .defaults(AgentSelection {
                agent_loop: Some("framework.loop".into()),
                ..AgentSelection::default()
            })
            .plugin(ConversationStorePlugin::new(UnusedConversations))
            .plugin(AgentLoopPlugin::new(AdmissionProbe {
                child: child.clone(),
                closed,
                dispatched,
                journal: Arc::new(AdmissionJournal {
                    mode,
                    snapshots: Mutex::new(Vec::new()),
                    entered: tokio::sync::Notify::new(),
                }),
            }))
            .build()
            .await
            .unwrap()
            .agent(AgentSelection::default())
            .build()
            .await
            .unwrap();
        let mut request = RunRequest::new("parent", "input");
        request.limits.max_total_tokens = 100;
        request.limits.max_child_runs = 1;
        tokio::time::timeout(std::time::Duration::from_secs(2), parent.run(request))
            .await
            .unwrap()
            .unwrap();
        parent.close().await;
        child.close().await;
    }
}

struct UnusedConversations;
#[async_trait]
impl ConversationStore for UnusedConversations {
    async fn open(&self, _: &str) -> Result<Box<dyn ConversationSession>, ConversationError> {
        panic!("custom admission loops do not access conversations")
    }
}
