use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use eliteforge_agent_core as fw;
use eliteforge_agent_official_plugin_agent_external::{
    JsonAgentBridge, JsonBridgeExternalAgent, JsonBridgeProjection,
};
use serde_json::json;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::Notify,
};

struct Projection;
impl JsonBridgeProjection for Projection {
    fn request(
        &self,
        request: &fw::ExternalAgentRequest,
    ) -> Result<serde_json::Value, fw::AgentError> {
        Ok(json!({
            "input": request.input.text(),
            "sessionId": request.session.as_ref().map(|session| &session.session_id),
        }))
    }

    fn observations(&self, _: &serde_json::Value) -> Vec<fw::ExternalAgentEvent> {
        vec![fw::ExternalAgentEvent::Status("response received".into())]
    }

    fn result(
        &self,
        response: serde_json::Value,
    ) -> Result<fw::ExternalAgentResult, fw::AgentError> {
        Ok(fw::ExternalAgentResult {
            content: response["output"].as_str().unwrap().into(),
            structured: None,
            session: Some(fw::ExternalSessionRef {
                session_id: response["sessionId"].as_str().unwrap().into(),
                run_id: response["runId"].as_str().map(str::to_owned),
            }),
            usage: fw::Usage::default(),
        })
    }
}

#[derive(Default)]
struct Events(Mutex<Vec<fw::ExternalAgentEvent>>);
#[async_trait::async_trait]
impl fw::ExternalAgentEventSink for Events {
    async fn emit(&self, event: fw::ExternalAgentEvent) {
        self.0.lock().unwrap().push(event);
    }
}

#[tokio::test]
async fn bridge_agent_routes_start_and_native_session_continuation() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    let routes = Arc::new(Mutex::new(Vec::new()));
    let server = {
        let routes = routes.clone();
        tokio::spawn(async move {
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut request_line = String::new();
                reader.read_line(&mut request_line).await.unwrap();
                routes.lock().unwrap().push(request_line);
                let mut stream = reader.into_inner();
                let body = r#"{"data":{"output":"done","sessionId":"native","runId":"run-1"}}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        })
    };
    let bridge = JsonAgentBridge::new(&endpoint, reqwest::Client::new()).unwrap();
    let agent = JsonBridgeExternalAgent::new(bridge, Arc::new(Projection));
    let events = Events::default();
    let first = fw::ExternalAgent::run(
        &agent,
        fw::ExternalAgentRequest {
            input: "start".into(),
            model: None,
            session: None,
            extensions: fw::Extensions::default(),
        },
        &events,
    )
    .await
    .unwrap();
    assert_eq!(first.content.text(), "done");
    assert_eq!(
        first.session.as_ref().unwrap().run_id.as_deref(),
        Some("run-1")
    );
    let _ = fw::ExternalAgent::run(
        &agent,
        fw::ExternalAgentRequest {
            input: "continue".into(),
            model: None,
            session: first.session,
            extensions: fw::Extensions::default(),
        },
        &events,
    )
    .await
    .unwrap();
    server.await.unwrap();
    let routes = routes.lock().unwrap();
    assert!(routes[0].contains("/runs/start"));
    assert!(routes[1].contains("/runs/resume"));
    assert_eq!(events.0.lock().unwrap().len(), 2);
}

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
                let (stream, _) = listener.accept().await.unwrap();
                let started = started.clone();
                let cancelled = cancelled.clone();
                let cancel_count = cancel_count.clone();
                tokio::spawn(async move {
                    let mut reader = BufReader::new(stream);
                    let mut request_line = String::new();
                    reader.read_line(&mut request_line).await.unwrap();
                    let mut stream = reader.into_inner();
                    if request_line.contains("/runs/known/cancel") {
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

#[tokio::test]
async fn unresponsive_cancel_is_bounded_and_releases_the_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut request_line = String::new();
        reader.read_line(&mut request_line).await.unwrap();
        assert!(request_line.contains("/runs/known/cancel"));
        // Intentionally never respond. EOF proves the cancelled HTTP request
        // releases its socket, rather than merely abandoning a JoinHandle.
        let mut remaining = Vec::new();
        reader.read_to_end(&mut remaining).await.unwrap();
    });
    let bridge = JsonAgentBridge::new(&endpoint, reqwest::Client::new()).unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(7),
        bridge.cancel("known", &json!({})),
    )
    .await
    .expect("cancel must enforce its own finite cleanup timeout");
    assert!(result.is_err());
    tokio::time::timeout(std::time::Duration::from_secs(2), server)
        .await
        .expect("timed-out cancellation must release the connection")
        .unwrap();
}
