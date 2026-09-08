use std::{
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    time::Duration,
};

use futures::FutureExt;

type Shutdown =
    Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> + Send>;
enum Cleanup {
    Sync(Box<dyn FnOnce() + Send>),
    Async(Shutdown),
}

/// Failures collected while continuing to release all remaining resources.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShutdownReport {
    pub errors: Vec<String>,
    pub panics: usize,
    pub timeouts: usize,
}
impl ShutdownReport {
    pub fn is_success(&self) -> bool {
        self.errors.is_empty() && self.panics == 0 && self.timeouts == 0
    }
    pub fn merge(&mut self, other: Self) {
        self.errors.extend(other.errors);
        self.panics += other.panics;
        self.timeouts += other.timeouts;
    }
}

/// Reverse-order cleanup with explicit asynchronous shutdown and a synchronous
/// Drop fallback. Register an abort/cancel fallback immediately on acquisition,
/// then register graceful shutdown. Drop cannot await remote acknowledgements.
#[derive(Default)]
pub struct ResourceScope {
    callbacks: Vec<Cleanup>,
    report: ShutdownReport,
}
impl ResourceScope {
    pub fn on_cleanup(&mut self, callback: impl FnOnce() + Send + 'static) {
        self.callbacks.push(Cleanup::Sync(Box::new(callback)));
    }
    pub fn on_shutdown<F, Fut>(&mut self, callback: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        self.callbacks
            .push(Cleanup::Async(Box::new(|| Box::pin(callback()))));
    }
    /// The timeout applies to each async callback. A failure never skips later
    /// callbacks. If this future is dropped, unpolled callbacks remain owned and
    /// synchronous fallbacks still run when the scope is dropped or closed.
    pub async fn close_async(&mut self, timeout: Duration) -> ShutdownReport {
        while let Some(callback) = self.callbacks.pop() {
            match callback {
                Cleanup::Sync(callback) => {
                    if catch_unwind(AssertUnwindSafe(callback)).is_err() {
                        self.report.panics += 1;
                    }
                }
                Cleanup::Async(callback) => {
                    let future = AssertUnwindSafe(async { callback().await }).catch_unwind();
                    match tokio::time::timeout(timeout, future).await {
                        Ok(Ok(Ok(()))) => {}
                        Ok(Ok(Err(error))) => self.report.errors.push(error),
                        Ok(Err(_)) => self.report.panics += 1,
                        Err(_) => self.report.timeouts += 1,
                    }
                }
            }
        }
        self.report.clone()
    }
    pub fn close(&mut self) -> ShutdownReport {
        while let Some(callback) = self.callbacks.pop() {
            if let Cleanup::Sync(callback) = callback
                && catch_unwind(AssertUnwindSafe(callback)).is_err()
            {
                self.report.panics += 1;
            }
        }
        self.report.clone()
    }
}
impl Drop for ResourceScope {
    fn drop(&mut self) {
        self.close();
    }
}
