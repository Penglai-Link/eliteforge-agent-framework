use async_trait::async_trait;
use eliteforge_agent_context::*;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_storage_local::{
    FileArtifactLimits, FileArtifactStore, SqliteOptions, SqliteStateStore,
};
use eliteforge_agent_test_support::*;
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn private_tempdir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    dir
}

#[tokio::test]
async fn atomic_artifact_roundtrip_bounds_and_path_rejection() {
    let dir = private_tempdir();
    let store = FileArtifactStore::open(
        dir.path(),
        FileArtifactLimits {
            max_artifact_bytes: 1024,
            max_read_bytes: 7,
            max_in_flight: 2,
        },
    )
    .unwrap();
    let token = CancellationToken::new();
    let original = "多字节 UTF-8 boundary".as_bytes().to_vec();
    let metadata = store.put(original.clone(), token.clone()).await.unwrap();
    let reopened = FileArtifactStore::open(
        dir.path(),
        FileArtifactLimits {
            max_artifact_bytes: 1024,
            max_read_bytes: 7,
            max_in_flight: 1,
        },
    )
    .unwrap();
    let mut offset = 0;
    let mut bytes = Vec::new();
    loop {
        let chunk = reopened
            .read(&metadata.id, offset, 7, token.clone())
            .await
            .unwrap();
        assert!(chunk.bytes.len() <= 7);
        offset = chunk.next_offset();
        bytes.extend_from_slice(&chunk.bytes);
        if chunk.eof() {
            break;
        }
    }
    assert_eq!(bytes, original);
    assert!(matches!(
        store.read(&metadata.id, 0, 8, token.clone()).await,
        Err(ArtifactError::InvalidRange)
    ));
    assert!(matches!(
        store.put(vec![0; 1025], token.clone()).await,
        Err(ArtifactError::TooLarge)
    ));
    for path in [
        "../secret",
        "/tmp/secret",
        "artifact:abc",
        "550e8400-e29b-41d4-a716-446655440000/../secret",
    ] {
        assert!(ArtifactId::parse(path).is_err());
    }
    assert!(
        std::fs::read_dir(dir.path()).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp"))
    );
    token.cancel();
    assert!(matches!(
        store.put(vec![1], token).await,
        Err(ArtifactError::Cancelled)
    ));
    store
        .remove(&metadata.id, CancellationToken::new())
        .await
        .unwrap();
    assert!(
        store
            .read(&metadata.id, 0, 1, CancellationToken::new())
            .await
            .is_err()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn artifact_reads_reject_symlinks_and_non_regular_files() {
    use std::os::unix::fs::symlink;
    let dir = private_tempdir();
    let outside = tempfile::NamedTempFile::new().unwrap();
    let id = ArtifactId::parse(uuid::Uuid::new_v4().to_string()).unwrap();
    symlink(outside.path(), dir.path().join(id.as_str())).unwrap();
    let store = FileArtifactStore::open(dir.path(), FileArtifactLimits::default()).unwrap();
    assert!(
        store
            .read(&id, 0, 32, CancellationToken::new())
            .await
            .is_err()
    );
    std::fs::remove_file(dir.path().join(id.as_str())).unwrap();
    std::fs::create_dir(dir.path().join(id.as_str())).unwrap();
    assert!(
        store
            .read(&id, 0, 32, CancellationToken::new())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn offload_publishes_original_before_projection_and_preserves_semantics() {
    let dir = private_tempdir();
    let store =
        Arc::new(FileArtifactStore::open(dir.path(), FileArtifactLimits::default()).unwrap());
    let offloader = ToolOutputOffloader::new(store.clone(), 256, 32).unwrap();
    let mut output = ToolOutput::error("诊断信息".repeat(400))
        .request_input(InteractionRequest::input("choose", json!({"type":"string"})).unwrap());
    output.structured = Some(json!({"error_code":"E42"}));
    let original = output.clone();
    let metadata = offloader
        .offload(&mut output, CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(output.is_error, original.is_error);
    assert!(output.structured.is_none());
    assert_eq!(
        output.result_reference.as_ref().unwrap().uri,
        format!("artifact:{}", metadata.id)
    );
    assert_eq!(output.effect, original.effect);
    assert!(output.content.text().len() < original.content.text().len());
    let bytes = store
        .read(
            &metadata.id,
            0,
            store.max_read_bytes(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(bytes.eof());
    let restored: ToolOutput = serde_json::from_slice(&bytes.bytes).unwrap();
    assert_eq!(restored, original);
    let token = CancellationToken::new();
    token.cancel();
    let mut unchanged = original.clone();
    assert!(offloader.offload(&mut unchanged, token).await.is_err());
    assert_eq!(unchanged, original);
}

#[tokio::test]
async fn structured_only_and_combined_payloads_are_externalized_losslessly() {
    for large_content in [false, true] {
        let dir = private_tempdir();
        let store =
            Arc::new(FileArtifactStore::open(dir.path(), FileArtifactLimits::default()).unwrap());
        let offloader = ToolOutputOffloader::new(store.clone(), 1024, 64).unwrap();
        let original = ToolOutput {
            content: if large_content {
                "large text".repeat(2048)
            } else {
                "done".into()
            }
            .into(),
            structured: Some(json!({"rows": "large-json".repeat(2048)})),
            is_error: true,
            effect: ToolEffect::ExposeTools(vec!["details".into()]),
            ..ToolOutput::default()
        };
        let mut output = original.clone();
        let metadata = offloader
            .offload(&mut output, CancellationToken::new())
            .await
            .unwrap()
            .unwrap();
        assert!(output.structured.is_none());
        assert_eq!(output.effect, original.effect);
        assert_eq!(output.is_error, original.is_error);
        assert_eq!(
            output.result_reference.as_ref().unwrap().uri,
            format!("artifact:{}", metadata.id)
        );
        assert!(serde_json::to_vec(&output).unwrap().len() < 1024);
        let chunk = store
            .read(
                &metadata.id,
                0,
                store.max_read_bytes(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(chunk.eof());
        assert_eq!(
            serde_json::from_slice::<ToolOutput>(&chunk.bytes).unwrap(),
            original
        );
    }
}

struct LargeStructuredTool;
#[async_trait]
impl Tool for LargeStructuredTool {
    async fn call(&self, _: serde_json::Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            structured: Some(json!({"rows": "x".repeat(30_000)})),
            ..ToolOutput::text("done")
        })
    }
}

struct ReferenceModel;
#[async_trait]
impl ChatModel for ReferenceModel {
    async fn generate(
        &self,
        _model_id: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        if let Some(message) = request.messages.iter().find(|m| m.role == Role::Tool) {
            let result = message.tool_result.as_ref().unwrap();
            assert!(result.structured.is_none());
            assert!(
                result
                    .result_reference
                    .as_ref()
                    .unwrap()
                    .uri
                    .starts_with("artifact:")
            );
            assert!(serde_json::to_vec(&request.messages).unwrap().len() < 2048);
            return Ok(ChatResponse::text("stored"));
        }
        Ok(ChatResponse::tool_calls(vec![ToolCall::new(
            "call",
            "large",
            json!({}),
        )]))
    }
}

#[tokio::test]
async fn loop_persists_reference_instead_of_large_structured_data() {
    let dir = private_tempdir();
    let artifacts =
        Arc::new(FileArtifactStore::open(dir.path(), FileArtifactLimits::default()).unwrap());
    let conversations = TestConversationStore::default();
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(ReferenceModel))
        .plugin(ConversationStorePlugin::new(conversations.clone()))
        .plugin(ToolMiddlewarePlugin::new(
            "offload",
            ToolOutputOffloader::new(artifacts.clone(), 1024, 64).unwrap(),
        ))
        .plugin(ToolProviderPlugin::new(
            "tools",
            StaticToolProvider::new(vec![ToolBinding::new(
                ToolDescriptor::new("large", "large", "large result", json!({"type":"object"})),
                LargeStructuredTool,
            )]),
        ))
        .build()
        .await
        .unwrap();
    assert_eq!(
        agent
            .run(RunRequest::new("s", "get rows"))
            .await
            .unwrap()
            .output,
        "stored"
    );
    let history = conversations.snapshot("s");
    assert_eq!(history[1].tool_calls[0].id, "call");
    assert_eq!(history[2].tool_call_id.as_deref(), Some("call"));
    let reference = history[2]
        .tool_result
        .as_ref()
        .unwrap()
        .result_reference
        .as_ref()
        .unwrap();
    let id =
        serde_json::from_value(json!(reference.uri.strip_prefix("artifact:").unwrap())).unwrap();
    let chunk = artifacts
        .read(&id, 0, artifacts.max_read_bytes(), CancellationToken::new())
        .await
        .unwrap();
    let original: ToolOutput = serde_json::from_slice(&chunk.bytes).unwrap();
    assert_eq!(
        original.structured.unwrap()["rows"].as_str().unwrap().len(),
        30_000
    );
    assert!(agent.framework().close().await.is_success());
}

struct CountingLargeTool(Arc<std::sync::atomic::AtomicUsize>);
#[async_trait]
impl Tool for CountingLargeTool {
    async fn call(&self, _: serde_json::Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ToolOutput {
            structured: Some(json!({"rows":"x".repeat(30000)})),
            ..ToolOutput::text("completed")
        })
    }
}
struct FailingArtifacts;
#[async_trait]
impl ArtifactStore for FailingArtifacts {
    fn max_artifact_bytes(&self) -> usize {
        64000
    }
    fn max_read_bytes(&self) -> usize {
        64000
    }
    async fn put(
        &self,
        _: Vec<u8>,
        _: CancellationToken,
    ) -> Result<ArtifactMetadata, ArtifactError> {
        Err(ArtifactError::Io(std::io::Error::other(
            "fixture write failure",
        )))
    }
    async fn read(
        &self,
        _: &ArtifactId,
        _: u64,
        _: usize,
        _: CancellationToken,
    ) -> Result<ArtifactChunk, ArtifactError> {
        Err(ArtifactError::NotFound)
    }
    async fn remove(&self, _: &ArtifactId, _: CancellationToken) -> Result<(), ArtifactError> {
        Err(ArtifactError::NotFound)
    }
}
fn offload_agent_builder(
    artifacts: Arc<dyn ArtifactStore>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
) -> TestBuilder {
    TestBuilder::default()
        .plugin(ChatModelPlugin::new(ReferenceModel))
        .plugin(ToolMiddlewarePlugin::new(
            "offload",
            ToolOutputOffloader::new(artifacts, 1024, 64).unwrap(),
        ))
        .plugin(ToolProviderPlugin::new(
            "tools",
            StaticToolProvider::new(vec![ToolBinding::new(
                ToolDescriptor::new("large", "large", "large result", json!({"type":"object"})),
                CountingLargeTool(calls),
            )]),
        ))
}
#[tokio::test]
async fn workflow_offload_failure_ends_run_without_automatic_tool_replay() {
    let directory = private_tempdir();
    let state = Arc::new(
        SqliteStateStore::open(directory.path().join("state.db"), SqliteOptions::default())
            .await
            .unwrap(),
    );
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let agent = offload_agent_builder(Arc::new(FailingArtifacts), calls.clone())
        .plugin(StateStorePlugin(state.clone()))
        .plugin(AgentLoopPlugin::new(ReactAgentLoop))
        .build()
        .await
        .unwrap();
    assert!(matches!(
        agent.run(RunRequest::new("s", "get rows")).await,
        Err(AgentError::Run(_))
    ));
    let checkpoint = state.latest("s").await.unwrap().unwrap();
    assert_eq!(checkpoint.status, ExecutionStatus::Failed);
    let mut request = RunRequest::new("s", "ignored");
    request.resume = Some(ResumeRequest {
        run_id: checkpoint.run_id,
        ..Default::default()
    });
    assert!(matches!(agent.run(request).await, Err(AgentError::Run(_))));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(
        !serde_json::to_string(&checkpoint.transcript)
            .unwrap()
            .contains("artifact:")
    );
    agent.framework().close().await;
    state.close().await.unwrap();
}
#[tokio::test]
async fn completed_workflow_replays_offloaded_reference_without_rerunning_tool() {
    let directory = private_tempdir();
    let artifacts_dir = directory.path().join("artifacts");
    std::fs::create_dir(&artifacts_dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&artifacts_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let artifacts =
        Arc::new(FileArtifactStore::open(&artifacts_dir, FileArtifactLimits::default()).unwrap());
    let state = Arc::new(
        SqliteStateStore::open(directory.path().join("state.db"), SqliteOptions::default())
            .await
            .unwrap(),
    );
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let agent = offload_agent_builder(artifacts.clone(), calls.clone())
        .plugin(StateStorePlugin(state.clone()))
        .plugin(AgentLoopPlugin::new(ReactAgentLoop))
        .build()
        .await
        .unwrap();
    agent.run(RunRequest::new("s", "get rows")).await.unwrap();
    let checkpoint = state.latest("s").await.unwrap().unwrap();
    let result = checkpoint
        .transcript
        .iter()
        .find(|message| message.role == Role::Tool)
        .unwrap()
        .tool_result
        .as_ref()
        .unwrap();
    assert!(result.structured.is_none());
    let id = ArtifactId::parse(
        result
            .result_reference
            .as_ref()
            .unwrap()
            .uri
            .strip_prefix("artifact:")
            .unwrap(),
    )
    .unwrap();
    let mut request = RunRequest::new("s", "ignored");
    request.resume = Some(ResumeRequest {
        run_id: checkpoint.run_id,
        ..Default::default()
    });
    assert_eq!(agent.run(request).await.unwrap().output, "stored");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(
        artifacts
            .read(&id, 0, artifacts.max_read_bytes(), CancellationToken::new())
            .await
            .unwrap()
            .eof()
    );
    let other_dir = private_tempdir();
    let other_store =
        FileArtifactStore::open(other_dir.path(), FileArtifactLimits::default()).unwrap();
    assert!(
        matches!(
            other_store.read(&id, 0, 1, CancellationToken::new()).await,
            Err(ArtifactError::NotFound)
        ),
        "IDs do not authorize access to another store"
    );
    artifacts
        .remove(&id, CancellationToken::new())
        .await
        .unwrap();
    assert!(matches!(
        artifacts.read(&id, 0, 1, CancellationToken::new()).await,
        Err(ArtifactError::NotFound)
    ));
    agent.framework().close().await;
    state.close().await.unwrap();
}
struct RejectToolHistory(TestConversationStore);
struct RejectToolSession(Box<dyn ConversationSession>);
#[async_trait]
impl ConversationStore for RejectToolHistory {
    async fn open(&self, id: &str) -> Result<Box<dyn ConversationSession>, ConversationError> {
        Ok(Box::new(RejectToolSession(self.0.open(id).await?)))
    }
}
#[async_trait]
impl ConversationSession for RejectToolSession {
    async fn snapshot(&mut self) -> Result<ConversationSnapshot, ConversationError> {
        self.0.snapshot().await
    }
    async fn append(&mut self, messages: &[Message]) -> Result<(), ConversationError> {
        if messages.iter().any(|message| message.role == Role::Tool) {
            return Err(ConversationError::Other(
                "fixture history write failure".into(),
            ));
        }
        self.0.append(messages).await
    }
}
#[tokio::test]
async fn history_failure_after_artifact_publish_preserves_original_without_replay() {
    let directory = private_tempdir();
    let artifacts =
        Arc::new(FileArtifactStore::open(directory.path(), FileArtifactLimits::default()).unwrap());
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let history = TestConversationStore::default();
    let agent = offload_agent_builder(artifacts, calls.clone())
        .plugin(ConversationStorePlugin::new(RejectToolHistory(
            history.clone(),
        )))
        .build()
        .await
        .unwrap();
    assert!(agent.run(RunRequest::new("s", "get rows")).await.is_err());
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(history.snapshot("s"), [Message::user("get rows")]);
    let files: Vec<_> = std::fs::read_dir(directory.path())
        .unwrap()
        .map(|file| file.unwrap())
        .collect();
    assert_eq!(files.len(), 1);
    let original: ToolOutput =
        serde_json::from_slice(&std::fs::read(files[0].path()).unwrap()).unwrap();
    assert_eq!(
        original.structured.unwrap()["rows"].as_str().unwrap().len(),
        30000
    );
    agent.framework().close().await;
}
