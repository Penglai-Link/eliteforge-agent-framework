//! Version-1 wire compatibility is adapted into the unified native assembler.
use async_trait::async_trait;
use eliteforge_agent_framework::*;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<ChatRequest>>>);
#[async_trait]
impl ChatModel for Capture {
    async fn generate(
        &self,
        _: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        self.0.lock().unwrap().push(request);
        Ok(ChatResponse::text("done"))
    }
}
#[derive(Clone, Default)]
struct Reports(Arc<Mutex<Vec<ContextAssemblyReport>>>);
#[async_trait]
impl EventSink for Reports {
    async fn on_event(&self, _: &RunContext, event: &AgentEvent) {
        if let AgentEvent::ContextAssembled(report) = event {
            self.0.lock().unwrap().push(report.clone());
        }
    }
}
#[tokio::test]
async fn version_one_provider_wire_installs_as_a_source_and_is_assembled_once() {
    let directory = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    let manager = CapabilityPackManager::new(directory.path()).unwrap();
    let mut manifest: Value = serde_json::from_str(include_str!(
        "../../../../examples/capability-pack/manifest.json"
    ))
    .unwrap();
    let declaration = manifest["implementations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|value| value["id"] == "demo.context")
        .unwrap();
    assert_eq!(declaration["extension"], "framework.context-provider");
    assert_eq!(declaration["contract"], "^1");
    let executable = if cfg!(windows) {
        "bin/backend.exe"
    } else {
        "bin/backend"
    };
    manifest["backend"]["executables"] = json!({platform_key(): executable});
    std::fs::create_dir_all(source.path().join("bin")).unwrap();
    std::fs::create_dir_all(source.path().join("ui")).unwrap();
    std::fs::copy(
        env!("CARGO_BIN_EXE_capability-backend"),
        source.path().join(executable),
    )
    .unwrap();
    std::fs::write(source.path().join("ui/index.html"), "<p>Fixture</p>").unwrap();
    std::fs::write(
        source.path().join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    manager
        .install_bytes(
            &build_capability_archive(source.path(), None).unwrap(),
            InstallPolicy::Development,
        )
        .unwrap();
    let selection = manager
        .select(
            "demo.pack",
            &"=1.0.0".parse().unwrap(),
            "context-wire",
            "v1",
            Value::Null,
        )
        .unwrap()
        .implementations([
            "demo.state",
            "demo.interactions",
            "demo.conversations",
            "demo.context",
            "demo.contributor",
        ])
        .unwrap();
    let capture = Capture::default();
    let reports = Reports::default();
    let agent = AgentBuilder::new()
        .default_models(false)
        .capability_pack(selection)
        .plugin(ChatModelPlugin::new(capture.clone()))
        .plugin(EventPlugin::new("reports", reports.clone()))
        .plugin(ContextLayoutPlugin(ContextLayout(vec![
            ContextBlockConfig::new(INSTRUCTIONS_BLOCK, BlockBudget::tokens(200)),
            ContextBlockConfig::new("remote", BlockBudget::tokens(200)),
            ContextBlockConfig::new(HISTORY_BLOCK, BlockBudget::tokens(200)),
        ])))
        .build()
        .await
        .unwrap();
    agent
        .run(RunRequest::new("legacy-wire-session", "question"))
        .await
        .unwrap();
    assert!(agent.close().await.is_success());
    let actual = capture.0.lock().unwrap();
    assert_eq!(actual.len(), 1);
    let texts = actual[0]
        .messages
        .iter()
        .map(|message| message.content.text())
        .collect::<Vec<_>>();
    assert_eq!(
        texts,
        ["SDK-provided context", "retrieved fact", "question"]
    );
    assert_eq!(actual[0].messages[0].role, Role::System);
    assert_eq!(actual[0].messages[1].role, Role::User);
    let reports = reports.0.lock().unwrap();
    assert_eq!(reports[0].blocks.len(), 3);
    assert_eq!(reports[0].blocks[0].kind, ContextSourceKind::Instruction);
    assert!(
        reports[0].blocks[0]
            .sources
            .iter()
            .any(|source| source.id == "demo.context")
    );
    assert_eq!(reports[0].blocks[1].kind, ContextSourceKind::Data);
    assert!(manager.usage().blockers("demo.pack").is_empty());
}
