//! Host-neutral local debugging server. Development installs require an explicit flag.
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use eliteforge_agent_framework::*;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
type ApiResult = Result<Json<Value>, (StatusCode, String)>;
fn error(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, error.to_string())
}
struct Host {
    manager: CapabilityPackManager,
    agent: tokio::sync::Mutex<Option<Agent>>,
    pages: tokio::sync::Mutex<BTreeMap<String, Arc<PageSession>>>,
    page_topics: tokio::sync::Mutex<BTreeMap<String, tokio::sync::mpsc::Receiver<PageMessage>>>,
    source: PathBuf,
    development: bool,
    dev_refresh: bool,
    revision: std::sync::atomic::AtomicU64,
    catalog: tokio::sync::Mutex<Option<(Value, Value)>>,
}
#[derive(Deserialize)]
struct Build {
    package: String,
    version: String,
    #[serde(default)]
    implementations: Option<Vec<String>>,
    #[serde(default)]
    configuration: Value,
    #[serde(default)]
    configuration_version: Option<String>,
}
#[derive(Deserialize)]
struct OpenPage {
    instance: InstanceIdentity,
    page: String,
    theme: String,
    language: String,
}
#[derive(Deserialize)]
struct CallPage {
    instance: InstanceIdentity,
    method: String,
    #[serde(default)]
    params: Value,
}
async fn catalog(State(host): State<Arc<Host>>) -> ApiResult {
    // Installed content is immutable. Cache the display snapshot; activation and
    // resource reads still revalidate the actual installed files.
    let mut cache = host.catalog.lock().await;
    if cache.is_none() {
        let manager = host.manager.clone();
        *cache=Some(tokio::task::spawn_blocking(move || -> Result<_,RuntimeError> {
            let packs=manager.installed()?;
            let extensions=manager.extension_catalog()?.into_iter().map(|entry|json!({"contract":entry.contract,"defining_package":entry.defining_package,"implementations":entry.implementations})).collect::<Vec<_>>();
            Ok((json!(packs),json!(extensions)))
        }).await.map_err(error)?.map_err(error)?);
    }
    let (packs, extensions) = cache.as_ref().unwrap().clone();
    drop(cache);
    Ok(Json(
        json!({"packs":packs,"extensions":extensions,"instances":host.manager.running_instances(),"selections":host.manager.running_selections(),"usage":host.manager.usage().all_blockers(),"dev_refresh":host.dev_refresh,"revision":host.revision.load(std::sync::atomic::Ordering::Acquire)}),
    ))
}
async fn import(State(host): State<Arc<Host>>, bytes: Bytes) -> ApiResult {
    let manager = host.manager.clone();
    let policy = if host.development {
        InstallPolicy::Development
    } else {
        InstallPolicy::Signed
    };
    let pack = tokio::task::spawn_blocking(move || manager.install_bytes(&bytes, policy))
        .await
        .map_err(error)?
        .map_err(error)?;
    *host.catalog.lock().await = None;
    Ok(Json(json!(pack)))
}
async fn build(State(host): State<Arc<Host>>, Json(input): Json<Build>) -> ApiResult {
    let mut selection = host
        .manager
        .select(
            &input.package,
            &format!("={}", input.version).parse().map_err(error)?,
            "debug.agent",
            input
                .configuration_version
                .unwrap_or_else(|| RunId::new_v4().to_string()),
            input.configuration,
        )
        .map_err(error)?;
    if let Some(ids) = input.implementations {
        selection = selection.implementations(ids).map_err(error)?;
    }
    let builder = AgentBuilder::new()
        .default_models(false)
        .capability_pack(selection);
    let agent = host.agent.lock().await.clone();
    if let Some(agent) = agent {
        agent.reconfigure(builder).await.map_err(error)?;
    } else {
        let agent = builder.build().await.map_err(error)?;
        let mut current = host.agent.lock().await;
        if current.is_some() {
            drop(current);
            agent.close().await;
            return Err(error("Agent was built concurrently"));
        }
        *current = Some(agent);
    }
    Ok(Json(json!({"ready":true})))
}
async fn dev_update(State(host): State<Arc<Host>>, input: Json<Build>) -> ApiResult {
    if !host.dev_refresh {
        return Err(error(
            "automatic refresh requires --development --dev-refresh",
        ));
    }
    let pages = std::mem::take(&mut *host.pages.lock().await);
    host.page_topics.lock().await.clear();
    for page in pages.values() {
        page.close();
    }
    let result = build(State(host.clone()), input).await?;
    host.revision
        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    Ok(result)
}
async fn run(State(host): State<Arc<Host>>, Json(input): Json<Value>) -> ApiResult {
    let agent = host
        .agent
        .lock()
        .await
        .clone()
        .ok_or_else(|| error("Build an Agent first"))?;
    let result = agent
        .run(RunRequest::new(
            "debug.session",
            input["input"].as_str().unwrap_or("hello"),
        ))
        .await
        .map_err(error)?;
    Ok(Json(json!(result)))
}
async fn open_page(State(host): State<Arc<Host>>, Json(input): Json<OpenPage>) -> ApiResult {
    let instance = host.manager.instance(&input.instance).map_err(error)?;
    let (page, topics) = PageSession::new(
        instance,
        &input.page,
        PageContext {
            theme: input.theme,
            language: input.language,
            data: Value::Null,
        },
        Default::default(),
        Arc::new(NoPageHost),
    )
    .map_err(error)?;
    host.page_topics
        .lock()
        .await
        .insert(page.id().into(), topics);
    let initialization = page.initialization();
    host.pages.lock().await.insert(page.id().into(), page);
    Ok(Json(initialization))
}
async fn call_page(
    State(host): State<Arc<Host>>,
    Path(session): Path<String>,
    Json(input): Json<CallPage>,
) -> ApiResult {
    let page = host
        .pages
        .lock()
        .await
        .get(&session)
        .cloned()
        .ok_or_else(|| error("Page session closed"))?;
    Ok(Json(
        page.call(&session, &input.instance, &input.method, input.params)
            .await
            .map_err(error)?,
    ))
}
async fn close_page(State(host): State<Arc<Host>>, Path(session): Path<String>) -> ApiResult {
    host.page_topics.lock().await.remove(&session);
    if let Some(page) = host.pages.lock().await.remove(&session) {
        page.close();
    }
    Ok(Json(Value::Null))
}
async fn page_events(
    State(host): State<Arc<Host>>,
    Path(session): Path<String>,
) -> Result<Response, (StatusCode, String)> {
    let topics = host
        .page_topics
        .lock()
        .await
        .remove(&session)
        .ok_or_else(|| error("page event channel closed or already connected"))?;
    let stream = futures::stream::unfold(topics, |mut topics| async move {
        topics.recv().await.map(|message| {
            (
                Ok::<_, std::convert::Infallible>(
                    axum::response::sse::Event::default()
                        .data(serde_json::to_string(&message).expect("serializable page message")),
                ),
                topics,
            )
        })
    });
    Ok(axum::response::Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::default())
        .into_response())
}
async fn resource(
    State(host): State<Arc<Host>>,
    Path((session, path)): Path<(String, String)>,
) -> Result<Response, (StatusCode, String)> {
    let page = host
        .pages
        .lock()
        .await
        .get(&session)
        .cloned()
        .ok_or_else(|| error("Page session closed"))?;
    let bytes = page.read_resource(&path).map_err(error)?;
    Ok(asset(&path, bytes))
}
fn asset(path: &str, bytes: Vec<u8>) -> Response {
    let mime = if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".js") {
        "text/javascript; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".json") {
        "application/json"
    } else {
        "application/octet-stream"
    };
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, mime.parse().unwrap());
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*".parse().unwrap());
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    (headers, bytes).into_response()
}
async fn files(
    State(host): State<Arc<Host>>,
    Path(path): Path<String>,
) -> Result<Response, (StatusCode, String)> {
    let relative = safe_relative(&path).map_err(error)?;
    if !path.starts_with("sdk/page/dist/") {
        return Err(error("asset path is not exposed by this host"));
    }
    let bytes = std::fs::read(host.source.join(relative)).map_err(error)?;
    Ok(asset(&path, bytes))
}
async fn index(State(host): State<Arc<Host>>) -> Result<Response, (StatusCode, String)> {
    Ok(asset(
        "index.html",
        std::fs::read(host.source.join("examples/capability-host/index.html")).map_err(error)?,
    ))
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().map(String::as_str) == Some("pack") {
        let source = PathBuf::from(args.get(1).ok_or("pack DIRECTORY ARCHIVE")?);
        let destination = args.get(2).ok_or("pack DIRECTORY ARCHIVE")?;
        let signing = if let Some(i) = args.iter().position(|arg| arg == "--sign-key") {
            let bytes = std::fs::read(args.get(i + 1).ok_or("missing signing key")?)?;
            Some(ed25519_dalek::SigningKey::from_bytes(
                &bytes
                    .try_into()
                    .map_err(|_| "signing key must contain 32 raw bytes")?,
            ))
        } else {
            None
        };
        let key_id = args
            .iter()
            .position(|arg| arg == "--key-id")
            .and_then(|i| args.get(i + 1));
        let signing = match signing.as_ref() {
            Some(key) => Some((
                key_id
                    .ok_or("--key-id is required for signed packages")?
                    .as_str(),
                key,
            )),
            None => None,
        };
        std::fs::write(destination, build_capability_archive(&source, signing)?)?;
        return Ok(());
    }
    let value = |name: &str, default: &str| {
        args.iter()
            .position(|arg| arg == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
            .unwrap_or_else(|| default.into())
    };
    let manager = CapabilityPackManager::new(value("--store", ".capability-dev/store"))?;
    let development = args.iter().any(|arg| arg == "--development");
    let dev_refresh = args.iter().any(|arg| arg == "--dev-refresh");
    if dev_refresh && !development {
        return Err("--dev-refresh requires --development".into());
    }
    let host = Arc::new(Host {
        manager,
        agent: Default::default(),
        pages: Default::default(),
        page_topics: Default::default(),
        source: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .canonicalize()?,
        development,
        dev_refresh,
        revision: std::sync::atomic::AtomicU64::new(0),
        catalog: tokio::sync::Mutex::new(None),
    });
    let app = Router::new()
        .route("/", get(index))
        .route("/api/catalog", get(catalog))
        .route("/api/import", post(import))
        .route("/api/build", post(build))
        .route("/api/dev/update", post(dev_update))
        .route("/api/run", post(run))
        .route("/api/pages", post(open_page))
        .route("/api/pages/{session}/call", post(call_page))
        .route("/api/pages/{session}/events", get(page_events))
        .route("/api/pages/{session}/close", post(close_page))
        .route("/resources/{session}/{*path}", get(resource))
        .route("/assets/{*path}", get(files))
        .layer(axum::extract::DefaultBodyLimit::max(128 * 1024 * 1024))
        .with_state(host.clone());
    let listener =
        tokio::net::TcpListener::bind(format!("127.0.0.1:{}", value("--port", "4317"))).await?;
    println!(
        "Capability development host: http://{}",
        listener.local_addr()?
    );
    let closing = host.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            let pages = std::mem::take(&mut *closing.pages.lock().await);
            for (_, page) in pages {
                page.close();
            }
            closing.page_topics.lock().await.clear();
            if let Some(agent) = closing.agent.lock().await.take() {
                agent.close().await;
            }
        })
        .await?;
    for page in host.pages.lock().await.values() {
        page.close();
    }
    if let Some(agent) = host.agent.lock().await.take() {
        agent.close().await;
    }
    Ok(())
}
