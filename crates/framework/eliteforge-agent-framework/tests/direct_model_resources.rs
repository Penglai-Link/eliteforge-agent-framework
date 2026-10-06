use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_plugin_core::ResourceScope;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

#[derive(Default)]
struct ResourceState {
    acquired: Notify,
    shutdown_started: Notify,
    acknowledge: Notify,
    shutdowns: AtomicUsize,
    fallbacks: AtomicUsize,
}
#[derive(Clone, Copy)]
enum Completion {
    Success,
    Wait,
    Fail,
}
struct ResourceWorkflow {
    state: Arc<ResourceState>,
    completion: Completion,
    wait_for_ack: bool,
    fail_cleanup: bool,
}
#[async_trait]
impl ModelWorkflow for ResourceWorkflow {
    async fn run(
        &self,
        context: &RunContext,
        _: &dyn ChatExecutor,
        _: &RunLimits,
        _: &ModelRef,
    ) -> Result<ChatResponse, AgentError> {
        let mut scope = ResourceScope::default();
        let state = self.state.clone();
        scope.on_cleanup(move || {
            state.fallbacks.fetch_add(1, Ordering::SeqCst);
        });
        let state = self.state.clone();
        let wait_for_ack = self.wait_for_ack;
        let fail_cleanup = self.fail_cleanup;
        scope.on_shutdown(move || async move {
            state.shutdown_started.notify_one();
            if wait_for_ack {
                state.acknowledge.notified().await;
            }
            state.shutdowns.fetch_add(1, Ordering::SeqCst);
            if fail_cleanup {
                Err("fixture shutdown failed".into())
            } else {
                Ok(())
            }
        });
        context.own_resources(scope);
        self.state.acquired.notify_one();
        match self.completion {
            Completion::Success => Ok(ChatResponse::text("complete")),
            Completion::Wait => std::future::pending().await,
            Completion::Fail => Err(AgentError::Run("workflow primary failure".into())),
        }
    }
}
struct UnusedModel;
#[async_trait]
impl ChatModel for UnusedModel {
    async fn generate(
        &self,
        _: &str,
        _: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        Err(ModelError::Fatal(
            "resource fixture must not dispatch".into(),
        ))
    }
}
fn client() -> ModelClient {
    ModelClient::new(ModelRuntimeUpdate::new(
        "resource-fixture",
        0,
        vec![ModelConnection::new(
            "fixture",
            Arc::new(UnusedModel),
            vec![ModelDescriptor::new("unused")],
        )],
    ))
    .unwrap()
}
fn limits() -> RunLimits {
    RunLimits {
        model_timeout: Duration::from_secs(2),
        run_timeout: Duration::from_secs(2),
        ..Default::default()
    }
}
async fn signalled(notify: &Notify) {
    tokio::time::timeout(Duration::from_secs(3), notify.notified())
        .await
        .expect("resource fixture must reach the expected boundary");
}
async fn close_owner(client: &ModelClient) {
    let report = tokio::time::timeout(Duration::from_secs(3), client.close_observed())
        .await
        .expect("owner drain must finish after the invocation releases admission");
    assert!(report.is_success());
}

#[tokio::test]
async fn success_cancellation_and_deadline_wait_for_resource_acknowledgement() {
    for mode in 0..3 {
        let owner = client();
        let state = Arc::new(ResourceState::default());
        let mut limits = limits();
        if mode == 2 {
            limits.run_timeout = Duration::from_millis(500);
        }
        let options = ModelInvocationOptions::for_call(&limits).unwrap();
        let cancel = options.cancellation.clone();
        let client = owner.with_options(options);
        let workflow = Arc::new(ResourceWorkflow {
            state: state.clone(),
            completion: if mode == 0 {
                Completion::Success
            } else {
                Completion::Wait
            },
            wait_for_ack: true,
            fail_cleanup: false,
        });
        let task = tokio::spawn(async move {
            client
                .workflow_observed(
                    &ModelRef::new("fixture", "unused"),
                    workflow,
                    &limits,
                    "resource-fixture",
                )
                .await
        });
        signalled(&state.acquired).await;
        if mode == 1 {
            cancel.cancel();
        }
        signalled(&state.shutdown_started).await;
        assert!(
            !task.is_finished(),
            "graceful release must await the acknowledgement"
        );
        assert_eq!(state.fallbacks.load(Ordering::SeqCst), 0);
        state.acknowledge.notify_one();
        let observed = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(observed.report.calls, 0);
        match mode {
            0 => assert_eq!(observed.result.unwrap().content.text(), "complete"),
            1 => assert!(matches!(
                observed.result.unwrap_err().primary(),
                AgentError::Cancelled
            )),
            _ => assert!(matches!(
                observed.result.unwrap_err().primary(),
                AgentError::Model(ModelError::Indeterminate(_))
            )),
        }
        assert_eq!(state.shutdowns.load(Ordering::SeqCst), 1);
        assert_eq!(state.fallbacks.load(Ordering::SeqCst), 1);
        close_owner(&owner).await;
    }
}

#[tokio::test]
async fn dropping_the_caller_runs_the_synchronous_resource_fallback() {
    let owner = client();
    let state = Arc::new(ResourceState::default());
    let client = owner.clone();
    let workflow = Arc::new(ResourceWorkflow {
        state: state.clone(),
        completion: Completion::Wait,
        wait_for_ack: true,
        fail_cleanup: false,
    });
    let task = tokio::spawn(async move {
        client
            .workflow_observed(
                &ModelRef::new("fixture", "unused"),
                workflow,
                &limits(),
                "resource-fixture",
            )
            .await
    });
    signalled(&state.acquired).await;
    task.abort();
    let error = tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .err()
        .expect("caller task must be cancelled");
    assert!(error.is_cancelled());
    assert_eq!(state.fallbacks.load(Ordering::SeqCst), 1);
    assert_eq!(state.shutdowns.load(Ordering::SeqCst), 0);
    close_owner(&owner).await;
}

#[tokio::test]
async fn failed_resource_release_preserves_primary_error_and_releases_owner_admission() {
    let owner = client();
    let state = Arc::new(ResourceState::default());
    let observed = owner
        .workflow_observed(
            &ModelRef::new("fixture", "unused"),
            Arc::new(ResourceWorkflow {
                state: state.clone(),
                completion: Completion::Fail,
                wait_for_ack: false,
                fail_cleanup: true,
            }),
            &limits(),
            "resource-fixture",
        )
        .await;
    assert_eq!(observed.report.calls, 0);
    let error = observed.result.unwrap_err();
    assert!(matches!(
        error.primary(),
        AgentError::Run(message) if message == "workflow primary failure"
    ));
    assert!(matches!(
        error,
        AgentError::WithCleanup { cleanup, .. }
            if matches!(cleanup.as_ref(), AgentError::Run(message)
                if message == "model resource cleanup failed")
    ));
    assert_eq!(state.shutdowns.load(Ordering::SeqCst), 1);
    assert_eq!(state.fallbacks.load(Ordering::SeqCst), 1);
    close_owner(&owner).await;
}

#[tokio::test]
async fn pending_resource_shutdown_obeys_the_cleanup_budget() {
    let owner = client();
    let state = Arc::new(ResourceState::default());
    let limits = RunLimits {
        model_timeout: Duration::from_secs(2),
        cleanup_timeout: Duration::from_millis(30),
        ..limits()
    };
    let observed = tokio::time::timeout(
        Duration::from_millis(500),
        owner.workflow_observed(
            &ModelRef::new("fixture", "unused"),
            Arc::new(ResourceWorkflow {
                state: state.clone(),
                completion: Completion::Success,
                wait_for_ack: true,
                fail_cleanup: false,
            }),
            &limits,
            "resource-fixture",
        ),
    )
    .await
    .expect("pending shutdown must use cleanup_timeout rather than model_timeout");
    assert!(matches!(
        observed.result.unwrap_err().primary(),
        AgentError::Run(message) if message == "model resource cleanup failed"
    ));
    assert_eq!(observed.report.calls, 0);
    assert_eq!(state.shutdowns.load(Ordering::SeqCst), 0);
    assert_eq!(state.fallbacks.load(Ordering::SeqCst), 1);
    close_owner(&owner).await;
}
