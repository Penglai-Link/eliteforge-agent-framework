use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use eliteforge_agent_official_plugin_agent_external::JsonAgentBridge;
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Notify,
};

#[tokio::test]
async fn dropping_wait_only_attempts_remote_cancel_with_a_known_run_id() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    let started = Arc::new(Notify::new());
    let cancelled = Arc::new(Notify::new());
    let cancel_count = Arc::new(AtomicUsize::new(0));
    let server = {
        let started = started.clone();
        let cancelled = cancelled.clone();
        let cancel_count = cancel_count.clone();
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let started = started.clone();
                let cancelled = cancelled.clone();
                let cancel_count = cancel_count.clone();
                tokio::spawn(async move {
                    let mut buffer = [0u8; 4096];
                    let n = stream.read(&mut buffer).await.unwrap();
                    let head = String::from_utf8_lossy(&buffer[..n]);
                    if head.contains("/runs/known/cancel") {
                        cancel_count.fetch_add(1, Ordering::SeqCst);
                        cancelled.notify_one();
                        let _ = stream
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
                            .await;
                    } else {
                        started.notify_one();
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                });
            }
        })
    };
    let bridge = JsonAgentBridge::new(&endpoint, reqwest::Client::new()).unwrap();
    let task = {
        let bridge = bridge.clone();
        tokio::spawn(async move {
            bridge
                .invoke_with_known_run(false, &json!({"input":"x"}), Some("known"), &json!({}))
                .await
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    task.abort();
    let _ = task.await;
    tokio::time::timeout(std::time::Duration::from_secs(2), cancelled.notified())
        .await
        .unwrap();
    assert_eq!(cancel_count.load(Ordering::SeqCst), 1);

    let task = {
        let bridge = bridge.clone();
        tokio::spawn(async move {
            bridge
                .invoke_with_known_run(false, &json!({"input":"y"}), None, &json!({}))
                .await
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    task.abort();
    let _ = task.await;
    assert_eq!(cancel_count.load(Ordering::SeqCst), 1);
    server.abort();
}
