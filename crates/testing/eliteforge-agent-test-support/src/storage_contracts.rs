//! Backend-neutral acceptance helpers: adapters supply SQL or RPC round-trip counters.
use std::{
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};
pub fn assert_query_budget(operation: &str, before: usize, after: usize, expected: usize) {
    assert_eq!(
        after - before,
        expected,
        "{operation}: storage round trips amplified"
    );
}
pub struct ReadLoad {
    pub milliseconds: Vec<f64>,
    pub elapsed_seconds: f64,
}
impl ReadLoad {
    pub fn p95_ms(&self) -> f64 {
        let mut values = self.milliseconds.clone();
        values.sort_by(f64::total_cmp);
        values[((values.len() - 1) * 95).div_ceil(100)]
    }
    pub fn assert_p95(&self, operation: &str, budget_ms: f64) {
        assert!(
            self.p95_ms() < budget_ms,
            "{operation} P95 {} ms exceeds {budget_ms} ms",
            self.p95_ms()
        );
    }
    pub fn report(&self, operation: &str) -> serde_json::Value {
        let mut values = self.milliseconds.clone();
        values.sort_by(f64::total_cmp);
        serde_json::json!({"suite":operation,"requests":values.len(),"p50Ms":values[(values.len()-1).div_ceil(2)],"p95Ms":self.p95_ms(),"maxMs":values.last(),"requestsPerSecond":values.len() as f64/self.elapsed_seconds})
    }
}
pub async fn concurrent_reads<F, Fut>(workers: usize, rounds: usize, read: F) -> ReadLoad
where
    F: Fn(usize, usize) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    assert!(workers > 0 && rounds > 0);
    let barrier = Arc::new(tokio::sync::Barrier::new(workers + 1));
    let read = Arc::new(read);
    let mut handles = Vec::new();
    for worker in 0..workers {
        let barrier = barrier.clone();
        let read = read.clone();
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            let mut times = Vec::new();
            for round in 0..rounds {
                let now = Instant::now();
                tokio::time::timeout(Duration::from_secs(10), read(worker, round))
                    .await
                    .expect("storage read timed out");
                times.push(now.elapsed().as_secs_f64() * 1000.);
            }
            times
        }));
    }
    let now = Instant::now();
    barrier.wait().await;
    let mut milliseconds = Vec::new();
    for handle in handles {
        milliseconds.extend(handle.await.expect("concurrent storage read failed"));
    }
    ReadLoad {
        milliseconds,
        elapsed_seconds: now.elapsed().as_secs_f64(),
    }
}
