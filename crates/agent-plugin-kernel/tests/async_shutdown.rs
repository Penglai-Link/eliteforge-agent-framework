use agent_plugin_kernel::*;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

#[tokio::test]
async fn reverse_shutdown_continues_after_failure_panic_and_timeout() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut scope = ResourceScope::default();
    let log = events.clone();
    scope.on_cleanup(move || log.lock().unwrap().push("fallback"));
    let log = events.clone();
    scope.on_shutdown(move || async move {
        log.lock().unwrap().push("graceful");
        Ok(())
    });
    scope.on_shutdown(|| async { Err("export failed".into()) });
    scope.on_shutdown(|| async {
        panic!("shutdown panic");
    });
    scope.on_shutdown(std::future::pending);
    let report = scope.close_async(Duration::from_millis(10)).await;
    assert_eq!(report.errors, ["export failed"]);
    assert_eq!((report.panics, report.timeouts), (1, 1));
    assert_eq!(*events.lock().unwrap(), ["graceful", "fallback"]);
    assert_eq!(scope.close_async(Duration::from_secs(1)).await, report);
}

#[tokio::test]
async fn dropping_interrupted_shutdown_executes_remaining_sync_fallback() {
    let log = Arc::new(Mutex::new(Vec::new()));
    {
        let mut scope = ResourceScope::default();
        let observed = log.clone();
        scope.on_cleanup(move || observed.lock().unwrap().push("released"));
        scope.on_shutdown(std::future::pending);
        assert!(
            tokio::time::timeout(
                Duration::from_millis(10),
                scope.close_async(Duration::from_secs(1))
            )
            .await
            .is_err()
        );
    }
    assert_eq!(*log.lock().unwrap(), ["released"]);
}

struct FailingPlugin(Arc<Mutex<Vec<&'static str>>>);
#[async_trait]
impl Plugin for FailingPlugin {
    fn id(&self) -> &str {
        "partial-installation"
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let log = self.0.clone();
        context.on_cleanup(move || log.lock().unwrap().push("fallback"));
        let log = self.0.clone();
        context.on_shutdown(move || async move {
            tokio::task::yield_now().await;
            log.lock().unwrap().push("shutdown");
            Ok(())
        });
        Err(PluginError::message("installation failed"))
    }
}

#[tokio::test]
async fn partial_installation_failure_awaits_registered_shutdown() {
    let log = Arc::new(Mutex::new(Vec::new()));
    assert!(
        PluginContainer::build(vec![Arc::new(FailingPlugin(log.clone()))])
            .await
            .is_err()
    );
    assert_eq!(*log.lock().unwrap(), ["shutdown", "fallback"]);
}
