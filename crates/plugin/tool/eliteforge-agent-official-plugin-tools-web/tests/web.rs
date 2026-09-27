mod support;
use eliteforge_agent_official_plugin_tools_web::*;
use serde_json::json;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use support::invoke;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Server {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn server() -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    let task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let captured = captured.clone();
            tokio::spawn(async move {
                let mut bytes = vec![0; 8192];
                let len = socket.read(&mut bytes).await.unwrap();
                let request = String::from_utf8_lossy(&bytes[..len]).to_string();
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
                captured.lock().unwrap().push(request);
                let (status, headers, body) = if path.starts_with("/search?") {
                    ("200 OK","Content-Type: application/json\r\n".to_owned(),r#"{"web":{"results":[{"title":"Result","url":"https://example.com/source","description":"verified snippet"}]}}"#.into())
                } else if path == "/redirect" {
                    (
                        "302 Found",
                        format!("Location: http://localhost:{}/html\r\n", address.port()),
                        String::new(),
                    )
                } else if path == "/large" {
                    ("200 OK", String::new(), "x".repeat(256))
                } else if path == "/slow" {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    ("200 OK", String::new(), "late".into())
                } else if path == "/error" {
                    (
                        "500 Internal Server Error",
                        String::new(),
                        "unavailable".into(),
                    )
                } else {
                    ("200 OK","Content-Type: text/html; charset=utf-8\r\n".into(),"<html><head><title>Example &amp; title</title><style>secret-css</style></head><body><h1>Hello</h1><p>World &amp; text</p><script>secret-script</script></body></html>".into())
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    Server {
        url: format!("http://{address}"),
        requests,
        task,
    }
}
fn fetch_plugin() -> WebToolsPlugin {
    WebToolsPlugin::new().fetch(WebFetchConfig::restricted(["127.0.0.1"]))
}

#[tokio::test]
async fn brave_protocol_and_html_fetch_preserve_sources_and_strip_scripts() {
    let server = server().await;
    let plugin = WebToolsPlugin::new().search(
        BraveSearch::with_endpoint("fixture-key", &format!("{}/search", server.url)).unwrap(),
    );
    let result = invoke(
        plugin,
        "web_search",
        json!({"query":"rust tools","limit":3}),
        false,
    )
    .await
    .unwrap()
    .structured
    .unwrap();
    assert_eq!(result["results"][0]["url"], "https://example.com/source");
    let requests = server.requests.lock().unwrap().clone();
    assert!(requests[0].contains("q=rust+tools&count=3"));
    assert!(
        requests[0]
            .to_lowercase()
            .contains("x-subscription-token: fixture-key")
    );
    let result = invoke(
        fetch_plugin(),
        "web_fetch",
        json!({"url":format!("{}/html",server.url)}),
        false,
    )
    .await
    .unwrap()
    .structured
    .unwrap();
    assert_eq!(result["title"], "Example & title");
    assert_eq!(result["text"], "Hello World & text");
    assert_eq!(result["sourceUrl"], format!("{}/html", server.url));
}

#[tokio::test]
async fn fetch_rechecks_redirects_rejects_oversize_and_cancels_or_times_out() {
    let server = server().await;
    for url in [
        "file:///etc/passwd".to_owned(),
        "http://user:pass@example.com/".into(),
        format!("{}/redirect", server.url),
    ] {
        assert!(
            invoke(fetch_plugin(), "web_fetch", json!({"url":url}), false)
                .await
                .is_err()
        );
    }
    assert_eq!(
        server.requests.lock().unwrap().len(),
        1,
        "disallowed redirect must not be followed"
    );
    let plugin =
        WebToolsPlugin::new().fetch(WebFetchConfig::restricted(["127.0.0.1"]).with_limits(32, 16));
    assert!(
        invoke(
            plugin,
            "web_fetch",
            json!({"url":format!("{}/large",server.url)}),
            false
        )
        .await
        .unwrap_err()
        .contains("byte limit")
    );
    assert!(
        invoke(
            fetch_plugin().with_timeout(Duration::from_millis(20)),
            "web_fetch",
            json!({"url":format!("{}/slow",server.url)}),
            false
        )
        .await
        .unwrap_err()
        .contains("timed out")
    );
    assert!(
        invoke(
            fetch_plugin(),
            "web_fetch",
            json!({"url":format!("{}/html",server.url)}),
            true
        )
        .await
        .unwrap_err()
        .contains("cancelled")
    );
    assert!(
        invoke(
            fetch_plugin(),
            "web_fetch",
            json!({"url":format!("{}/error",server.url)}),
            false
        )
        .await
        .unwrap_err()
        .contains("500")
    );
}
