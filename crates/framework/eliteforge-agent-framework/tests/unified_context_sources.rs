//! Real model-path coverage for the single context source/assembler contract.
use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_skills::{
    FileSystemSkillProvider, FileSystemSkillsConfig, SkillsPlugin,
};
use eliteforge_agent_test_support::TestBuilder;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Notify;

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
struct Source {
    id: &'static str,
    kind: ContextSourceKind,
    calls: Arc<AtomicUsize>,
    messages: Vec<Message>,
}
#[async_trait]
impl ContextContributor for Source {
    fn kind(&self) -> ContextSourceKind {
        self.kind
    }
    fn block_id(&self) -> &str {
        self.id
    }
    async fn contribute(&self, input: &ContextInput<'_>) -> Result<ContextBlock, AgentError> {
        input.run.check_cancelled()?;
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(input.current_input.text(), "question");
        assert!(input.budget.max_tokens <= 200);
        Ok(ContextBlock {
            id: self.id.into(),
            trust: if self.kind == ContextSourceKind::Instruction {
                ContextTrust::Instruction
            } else {
                ContextTrust::Data
            },
            messages: self.messages.clone(),
            source_version: Some("source-v1".into()),
        })
    }
}
#[tokio::test]
async fn six_logical_sources_share_one_read_contract_and_protected_roles() {
    let capture = Capture::default();
    let reports = Reports::default();
    let instruction_calls = Arc::new(AtomicUsize::new(0));
    let mut builder = TestBuilder::default()
        .plugin(ChatModelPlugin::new(capture.clone()))
        .plugin(EventPlugin::new("reports", reports.clone()))
        .plugin(ContextContributorPlugin::new(
            "host.instructions",
            Source {
                id: "instructions",
                kind: ContextSourceKind::Instruction,
                calls: instruction_calls.clone(),
                messages: vec![Message::system("trusted host rule")],
            },
        ));
    let mut calls = Vec::new();
    let mut blocks = vec![ContextBlockConfig::new(
        INSTRUCTIONS_BLOCK,
        BlockBudget::tokens(200),
    )];
    for id in ["application", "checkpoint", "family", "working"] {
        let count = Arc::new(AtomicUsize::new(0));
        calls.push(count.clone());
        blocks.push(ContextBlockConfig::new(id, BlockBudget::tokens(100)));
        builder = builder.plugin(ContextContributorPlugin::new(
            id,
            Source {
                id,
                kind: ContextSourceKind::Data,
                calls: count,
                messages: vec![Message::user(id)],
            },
        ));
    }
    blocks.push(ContextBlockConfig::new(
        HISTORY_BLOCK,
        BlockBudget::tokens(200),
    ));
    let agent = builder
        .plugin(ContextLayoutPlugin(ContextLayout(blocks)))
        .build()
        .await
        .unwrap();
    agent
        .run(RunRequest::new("six-sources", "question"))
        .await
        .unwrap();
    assert!(agent.framework().close().await.is_success());
    assert_eq!(instruction_calls.load(Ordering::SeqCst), 1);
    assert!(calls.iter().all(|count| count.load(Ordering::SeqCst) == 1));
    let requests = capture.0.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].messages[0].role, Role::System);
    assert_eq!(
        requests[0].messages[0].instruction_origin,
        Some(InstructionOrigin::Plugin)
    );
    assert_eq!(
        requests[0]
            .messages
            .iter()
            .map(|m| m.content.text())
            .collect::<Vec<_>>(),
        [
            "trusted host rule",
            "application",
            "checkpoint",
            "family",
            "working",
            "question"
        ]
    );
    let reports = reports.0.lock().unwrap();
    assert_eq!(reports[0].blocks.len(), 6);
    assert_eq!(reports[0].blocks[0].kind, ContextSourceKind::Instruction);
    assert_eq!(reports[0].blocks[5].kind, ContextSourceKind::History);
    assert!(
        reports[0].blocks[1..5]
            .iter()
            .all(|block| block.kind == ContextSourceKind::Data)
    );
    let provenance = &reports[0].blocks[0].sources;
    assert_eq!(provenance[1].id, "host.instructions");
    assert_eq!(provenance[1].source_version.as_deref(), Some("source-v1"));
}

#[tokio::test]
async fn agent_and_plugin_instructions_retain_distinct_origins() {
    let capture = Capture::default();
    let mut stage = WorkflowStage::new("work");
    stage
        .instructions
        .push(Message::system("agent custom rule"));
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(capture.clone()))
        .plugin(AgentLoopPlugin::new(
            WorkflowLoop::new(WorkflowDefinition {
                id: "instruction-origins".into(),
                version: 1,
                stages: vec![stage],
            })
            .unwrap(),
        ))
        .plugin(ContextLayoutPlugin(ContextLayout(vec![
            ContextBlockConfig::new(INSTRUCTIONS_BLOCK, BlockBudget::tokens(200)),
            ContextBlockConfig::new(HISTORY_BLOCK, BlockBudget::tokens(200)),
        ])))
        .plugin(ContextContributorPlugin::new(
            "plugin.instructions",
            Source {
                id: "plugin.instructions",
                kind: ContextSourceKind::Instruction,
                calls: Arc::new(AtomicUsize::new(0)),
                messages: vec![Message::system("plugin rule")],
            },
        ))
        .build()
        .await
        .unwrap();
    agent
        .run(RunRequest::new("work", "question"))
        .await
        .unwrap();
    assert!(agent.framework().close().await.is_success());
    let requests = capture.0.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].messages[0].content.text(), "agent custom rule");
    assert_eq!(
        requests[0].messages[0].instruction_origin,
        Some(InstructionOrigin::Agent)
    );
    assert_eq!(requests[0].messages[1].content.text(), "plugin rule");
    assert_eq!(
        requests[0].messages[1].instruction_origin,
        Some(InstructionOrigin::Plugin)
    );
    assert_eq!(requests[0].messages[2].instruction_origin, None);
}

struct Preparation;
#[async_trait]
impl ContextPreparationPolicy for Preparation {
    async fn prepare(
        &self,
        input: &ContextAssemblyInput<'_>,
        _: usize,
    ) -> Result<ContextPreparation, AgentError> {
        DefaultContextAssembler::default()
            .assemble(input)
            .await
            .map(ContextPreparation::Ready)
    }
}
#[tokio::test]
async fn skills_compose_once_into_builtin_instructions_with_custom_preparation() {
    let directory = tempfile::tempdir().unwrap();
    let skill = directory.path().join("review");
    std::fs::create_dir(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), "---\nname: review\ndescription: Review the application.\n---\nPRIVATE_LOADED_INSTRUCTION_SENTINEL").unwrap();
    let provider = FileSystemSkillProvider::new(
        FileSystemSkillsConfig::new()
            .with_system_directory(directory.path())
            .without_local_directory(),
    );
    let capture = Capture::default();
    let reports = Reports::default();
    let agent = TestBuilder::default()
        .skills(["review"])
        .plugin(ChatModelPlugin::new(capture.clone()))
        .plugin(EventPlugin::new("reports", reports.clone()))
        .plugin(SkillsPlugin::new(provider))
        .plugin(ContextAssemblerPlugin::new(PreparingContextAssembler::new(
            Preparation,
            2,
        )))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("skills-once", "question");
    request.extensions.insert(ManagedDeveloperPrompts(std::collections::BTreeMap::from([
        ("skills.guidance".into(), "managed skill rule".into()),
    ])));
    agent
        .run(request)
        .await
        .unwrap();
    assert!(agent.framework().close().await.is_success());
    let requests = capture.0.lock().unwrap();
    let text = requests[0]
        .messages
        .iter()
        .map(|m| m.content.text())
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(text.matches("<available_skills>").count(), 1);
    assert!(text.contains("managed skill rule"));
    assert!(!text.contains("PRIVATE_LOADED_INSTRUCTION_SENTINEL"));
    let catalog_index = requests[0].messages.iter().position(|message|
        message.content.text().contains("<available_skills>"))
        .expect("skill catalog message");
    assert_eq!(requests[0].messages[catalog_index].role, Role::User);
    assert_eq!(requests[0].messages[catalog_index].instruction_origin, None);
    assert_eq!(requests[0].messages[catalog_index - 1].role, Role::System);
    assert_eq!(requests[0].messages[catalog_index - 1].instruction_origin,
        Some(InstructionOrigin::Plugin));
    let reports = reports.0.lock().unwrap();
    assert_eq!(reports[0].blocks.len(), 2);
    assert!(
        reports[0].blocks[0]
            .sources
            .iter()
            .any(|source| source.id == "skills.catalog")
    );
}

struct PendingInstructions(Arc<Notify>);
#[async_trait]
impl ContextContributor for PendingInstructions {
    fn kind(&self) -> ContextSourceKind {
        ContextSourceKind::Instruction
    }
    fn block_id(&self) -> &str {
        "pending.instructions"
    }
    async fn contribute(&self, _: &ContextInput<'_>) -> Result<ContextBlock, AgentError> {
        self.0.notify_one();
        std::future::pending().await
    }
}
#[tokio::test]
async fn instruction_source_read_obeys_cancellation_before_model_admission() {
    let entered = Arc::new(Notify::new());
    let capture = Capture::default();
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(capture.clone()))
        .plugin(ContextContributorPlugin::new(
            "pending",
            PendingInstructions(entered.clone()),
        ))
        .build()
        .await
        .unwrap();
    let handle = agent
        .start(RunRequest::new("cancel-source", "question"))
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    agent.cancel(&handle).unwrap();
    let error = tokio::time::timeout(std::time::Duration::from_secs(5), agent.wait(handle))
        .await
        .unwrap()
        .unwrap_err();
    assert!(matches!(error.primary(), AgentError::Cancelled));
    assert!(capture.0.lock().unwrap().is_empty());
    assert!(agent.framework().close().await.is_success());
}
#[tokio::test]
async fn instruction_quota_failure_keeps_provenance_and_never_admits_model() {
    let capture = Capture::default();
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(capture.clone()))
        .plugin(ContextContributorPlugin::new(
            "oversized.instructions",
            FixedInstructions::new("source", "x".repeat(10_000)),
        ))
        .plugin(ContextLayoutPlugin(ContextLayout(vec![
            ContextBlockConfig::new(INSTRUCTIONS_BLOCK, BlockBudget::tokens(50)),
            ContextBlockConfig::new(HISTORY_BLOCK, BlockBudget::tokens(100)),
        ])))
        .build()
        .await
        .unwrap();
    let error = agent
        .run(RunRequest::new("quota-source", "question"))
        .await
        .unwrap_err();
    let AgentError::ContextAssembly { report, .. } = error else {
        panic!("source failure must include the assembly report");
    };
    assert_eq!(report.blocks[0].kind, ContextSourceKind::Instruction);
    assert!(
        report.blocks[0]
            .sources
            .iter()
            .any(|source| source.id == "oversized.instructions")
    );
    assert!(capture.0.lock().unwrap().is_empty());
    assert!(agent.framework().close().await.is_success());
}
