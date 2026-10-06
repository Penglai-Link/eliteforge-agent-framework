//! Acceptance coverage for cross-crate capability discovery and assembly.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use eliteforge_agent_capability_test_fixture as fixture;
use eliteforge_agent_context::{
    ArtifactChunk, ArtifactError, ArtifactId, ArtifactMetadata, ArtifactStore,
    ArtifactStoreService, ConversationSummary, SummaryKey, SummaryStore, SummaryStoreService,
    VersionedSummary,
};
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_skills::SkillsPlugin;
use eliteforge_agent_test_support::TestConversationStore;
use serde_json::json;

fn builder(agent_loop: impl AgentLoop + 'static) -> AgentBuilder {
    AgentBuilder::new()
        .default_models(false)
        .plugin(AgentLoopPlugin::new(agent_loop))
        .plugin(ConversationStorePlugin::new(
            TestConversationStore::default(),
        ))
}

fn complete_result() -> RunResult {
    RunResult {
        output: String::new(),
        content: Content::default(),
        structured: None,
        status: RunStatus::Completed,
        interactions: Vec::new(),
        usage: Usage::default(),
        steps: 0,
        tool_calls: 0,
        external_session: None,
    }
}

struct UnusedLoop;

#[async_trait]
impl AgentLoop for UnusedLoop {
    async fn run(
        &self,
        _: &RunRequest,
        _: &RunContext,
        _: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        unreachable!("these tests only build and close agents")
    }
}

#[derive(Default)]
struct DeclaredAgentLoop;

#[capability(pack = "declared-agent-loop")]
#[async_trait]
impl AgentLoop for DeclaredAgentLoop {
    async fn run(
        &self,
        _: &RunRequest,
        _: &RunContext,
        _: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        Ok(complete_result())
    }
}

#[derive(Default)]
struct DeclaredChatModel;

#[capability(pack = "declared-chat-model")]
#[async_trait]
impl ChatModel for DeclaredChatModel {
    async fn generate(
        &self,
        _: &str,
        _: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        Ok(ChatResponse::text("declared-chat-model"))
    }
}

use eliteforge_agent_framework::ContextContributor as AliasedContextContributor;

#[derive(Default)]
struct AliasedDeclaredContext;

#[capability(pack = "aliased-context-provider")]
#[async_trait]
impl AliasedContextContributor for AliasedDeclaredContext {
    fn kind(&self) -> ContextSourceKind {
        ContextSourceKind::Instruction
    }
    fn block_id(&self) -> &str {
        std::any::type_name::<Self>()
    }
    async fn contribute(&self, _input: &ContextInput<'_>) -> Result<ContextBlock, AgentError> {
        Ok(ContextBlock::instructions(
            self.block_id(),
            vec![Message::system("aliased context provider")],
        ))
    }
}

#[capability_factory(pack = "factory-skills-runtime")]
fn configured_skills_runtime() -> SkillsPlugin {
    SkillsPlugin::sources_only().only_skills(["factory-skill"])
}

struct InspectServiceCounts(Arc<Mutex<Vec<(usize, usize)>>>);

#[async_trait]
impl AgentLoop for InspectServiceCounts {
    async fn run(
        &self,
        _: &RunRequest,
        _: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((services.context_sources.len(), services.tools.len()));
        Ok(complete_result())
    }
}

struct InspectContexts(Arc<Mutex<Vec<String>>>);

#[async_trait]
impl AgentLoop for InspectContexts {
    async fn run(
        &self,
        _: &RunRequest,
        context: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let mut contents = Vec::new();
        for provider in &services.context_sources {
            contents.extend(
                provider
                    .contribute(&ContextInput {
                        stage: "test",
                        step: 1,
                        model: &ModelRef::new("test", "test"),
                        budget: ContextBudget {
                            max_tokens: 100_000,
                            max_messages: 4096,
                        },
                        current_input: &Content::default(),
                        history: &ConversationSnapshot::new(
                            ConversationKey::new(&context.session_id),
                            "test",
                            Vec::new(),
                        ),
                        estimator: &ConservativeTokenEstimator,
                        run: context,
                    })
                    .await?
                    .messages
                    .into_iter()
                    .map(|message| message.content.text()),
            );
        }
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend(contents);
        Ok(complete_result())
    }
}

struct InspectTools(Arc<Mutex<Vec<ToolDescriptor>>>);

#[async_trait]
impl AgentLoop for InspectTools {
    async fn run(
        &self,
        _: &RunRequest,
        context: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let mut descriptors = Vec::new();
        for provider in &services.tools {
            descriptors.extend(
                provider
                    .lease(context)
                    .await?
                    .tools
                    .into_iter()
                    .map(|binding| binding.descriptor),
            );
        }
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend(descriptors);
        Ok(complete_result())
    }
}

struct RecordedPlugin {
    id: &'static str,
    state: Arc<fixture::FixtureState>,
}

#[derive(Default)]
struct ExternalPaymentRule;

#[capability(pack = "custom-extension")]
impl fixture::PaymentRule for ExternalPaymentRule {
    fn id(&self) -> &str {
        "external-payment-rule"
    }
}

#[derive(Default)]
struct PrimaryCheckoutAuthority;

#[capability(
    pack = "single-extension.primary",
    id = "single-extension.primary.authority"
)]
impl fixture::CheckoutAuthority for PrimaryCheckoutAuthority {
    fn id(&self) -> &str {
        "primary-checkout-authority"
    }
}

#[derive(Default)]
struct SecondaryCheckoutAuthority;

#[capability(
    pack = "single-extension.secondary",
    id = "single-extension.secondary.authority"
)]
impl fixture::CheckoutAuthority for SecondaryCheckoutAuthority {
    fn id(&self) -> &str {
        "secondary-checkout-authority"
    }
}

struct PaymentRuleConsumer(Arc<Mutex<Vec<String>>>);

#[async_trait]
impl Plugin for PaymentRuleConsumer {
    fn id(&self) -> &str {
        "test.payment-rule-consumer"
    }

    async fn install(&self, _: &mut PluginContext<'_>) -> Result<(), PluginError> {
        Ok(())
    }

    async fn ready(&self, context: &mut PluginReadyContext<'_>) -> Result<(), PluginError> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend(
                context
                    .extensions::<dyn fixture::PaymentRule>()
                    .into_iter()
                    .map(|rule| rule.id().to_owned()),
            );
        Ok(())
    }
}

struct CheckoutAuthorityConsumer(Arc<Mutex<Vec<String>>>);

#[async_trait]
impl Plugin for CheckoutAuthorityConsumer {
    fn id(&self) -> &str {
        "test.checkout-authority-consumer"
    }

    async fn install(&self, _: &mut PluginContext<'_>) -> Result<(), PluginError> {
        Ok(())
    }

    async fn ready(&self, context: &mut PluginReadyContext<'_>) -> Result<(), PluginError> {
        let authority = context
            .extension::<dyn fixture::CheckoutAuthority>()
            .ok_or_else(|| PluginError::message("checkout authority is missing"))?;
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(authority.id().to_owned());
        Ok(())
    }
}

#[derive(Default)]
struct DeclaredSummaryStore;

#[capability(pack = "declared-context-summary-store")]
#[async_trait]
impl SummaryStore for DeclaredSummaryStore {
    async fn load(&self, _: &SummaryKey) -> Result<Option<VersionedSummary>, ContextError> {
        Ok(None)
    }

    async fn compare_exchange(
        &self,
        _: &SummaryKey,
        _: Option<&str>,
        _: &ConversationSummary,
    ) -> Result<bool, ContextError> {
        Ok(true)
    }

    async fn delete(&self, _: &SummaryKey, _: &str) -> Result<bool, ContextError> {
        Ok(true)
    }
}

#[derive(Default)]
struct DeclaredArtifactStore;

#[capability(pack = "declared-context-artifact-store")]
#[async_trait]
impl ArtifactStore for DeclaredArtifactStore {
    fn max_artifact_bytes(&self) -> usize {
        1024
    }

    fn max_read_bytes(&self) -> usize {
        1024
    }

    async fn put(
        &self,
        _: Vec<u8>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<ArtifactMetadata, ArtifactError> {
        Err(ArtifactError::NotFound)
    }

    async fn read(
        &self,
        _: &ArtifactId,
        _: u64,
        _: usize,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<ArtifactChunk, ArtifactError> {
        Err(ArtifactError::NotFound)
    }

    async fn remove(
        &self,
        _: &ArtifactId,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<(), ArtifactError> {
        Err(ArtifactError::NotFound)
    }
}

struct ContextStorageConsumer(Arc<Mutex<Vec<String>>>);

#[async_trait]
impl Plugin for ContextStorageConsumer {
    fn id(&self) -> &str {
        "test.context-storage-consumer"
    }

    async fn install(&self, _: &mut PluginContext<'_>) -> Result<(), PluginError> {
        Ok(())
    }

    async fn ready(&self, context: &mut PluginReadyContext<'_>) -> Result<(), PluginError> {
        if context.extension::<dyn SummaryStore>().is_none()
            || context.services().get::<SummaryStoreService>().is_none()
        {
            return Err(PluginError::message("summary store was not registered"));
        }
        if context.extension::<dyn ArtifactStore>().is_none()
            || context.services().get::<ArtifactStoreService>().is_none()
        {
            return Err(PluginError::message("artifact store was not registered"));
        }
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push("context-storage-ready".into());
        Ok(())
    }
}

#[derive(Default)]
struct DeclaredSkillProvider;

#[capability(pack = "declared-skill-provider")]
#[async_trait]
impl SkillProvider for DeclaredSkillProvider {
    async fn catalog(&self, _: &RunContext) -> Result<SkillCatalog, AgentError> {
        Ok(SkillCatalog {
            skills: vec![
                SkillSummary {
                    name: "declared-review".into(),
                    description: "Review a declaration supplied by a capability package.".into(),
                    source: "declared".into(),
                    location: "capability://declared-review".into(),
                    license: None,
                    compatibility: None,
                    metadata: Default::default(),
                    allowed_tools: None,
                },
                SkillSummary {
                    name: "declared-hidden".into(),
                    description: "A second capability-provided skill used for filtering coverage."
                        .into(),
                    source: "declared".into(),
                    location: "capability://declared-hidden".into(),
                    license: None,
                    compatibility: None,
                    metadata: Default::default(),
                    allowed_tools: None,
                },
            ],
            diagnostics: Vec::new(),
        })
    }

    async fn load(&self, _: &RunContext, name: &str) -> Result<SkillDetail, AgentError> {
        Err(AgentError::Run(format!(
            "test provider does not load skill `{name}`"
        )))
    }

    async fn read_resource(
        &self,
        _: &RunContext,
        name: &str,
        path: &str,
    ) -> Result<SkillResource, AgentError> {
        Err(AgentError::Run(format!(
            "test provider does not read `{name}/{path}`"
        )))
    }
}

#[derive(Default)]
struct DuplicateDeclaredSkillProvider;

#[capability(pack = "duplicate-declared-skill-provider")]
#[async_trait]
impl SkillProvider for DuplicateDeclaredSkillProvider {
    async fn catalog(&self, _: &RunContext) -> Result<SkillCatalog, AgentError> {
        Ok(SkillCatalog {
            skills: vec![SkillSummary {
                name: "declared-review".into(),
                description: "A deliberately colliding skill name.".into(),
                source: "duplicate".into(),
                location: "capability://duplicate-declared-review".into(),
                license: None,
                compatibility: None,
                metadata: Default::default(),
                allowed_tools: None,
            }],
            diagnostics: Vec::new(),
        })
    }

    async fn load(&self, _: &RunContext, name: &str) -> Result<SkillDetail, AgentError> {
        Err(AgentError::Run(format!(
            "duplicate test provider does not load skill `{name}`"
        )))
    }

    async fn read_resource(
        &self,
        _: &RunContext,
        name: &str,
        path: &str,
    ) -> Result<SkillResource, AgentError> {
        Err(AgentError::Run(format!(
            "duplicate test provider does not read `{name}/{path}`"
        )))
    }
}

struct FactoryPluginConfig(Arc<Mutex<Vec<String>>>);

struct FactoryCapabilityPlugin {
    events: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Plugin for FactoryCapabilityPlugin {
    fn id(&self) -> &str {
        "factory.inner"
    }

    async fn install(&self, _: &mut PluginContext<'_>) -> Result<(), PluginError> {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push("factory-plugin-installed".into());
        Ok(())
    }

    async fn ready(&self, _: &mut PluginReadyContext<'_>) -> Result<(), PluginError> {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push("factory-plugin-ready".into());
        Ok(())
    }
}

#[capability_factory(pack = "factory-capability", config = FactoryPluginConfig)]
fn factory_capability(config: &FactoryPluginConfig) -> FactoryCapabilityPlugin {
    FactoryCapabilityPlugin {
        events: config.0.clone(),
    }
}

struct ReadyFailureConfig(Arc<Mutex<Vec<String>>>);

struct ReadyFailureCapabilityPlugin {
    events: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Plugin for ReadyFailureCapabilityPlugin {
    fn id(&self) -> &str {
        "ready-failure.inner"
    }

    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push("install".into());
        let events = self.events.clone();
        context.on_cleanup(move || {
            events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push("cleanup".into());
        });
        Ok(())
    }

    async fn ready(&self, _: &mut PluginReadyContext<'_>) -> Result<(), PluginError> {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push("ready".into());
        Err(PluginError::message("deliberate ready failure"))
    }
}

#[capability_factory(
    pack = "ready-failure",
    id = "ready-failure.plugin",
    config = ReadyFailureConfig
)]
fn ready_failure_capability(config: &ReadyFailureConfig) -> ReadyFailureCapabilityPlugin {
    ReadyFailureCapabilityPlugin {
        events: config.0.clone(),
    }
}

#[derive(Clone)]
struct EmbeddingCapabilityConfig {
    connection: String,
    model: String,
}

struct DeclaredEmbeddingModel;

#[async_trait]
impl EmbeddingModel for DeclaredEmbeddingModel {
    async fn embed(
        &self,
        model: &str,
        request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, ModelError> {
        Ok(EmbeddingResponse {
            provider_model: None,
            model: ModelRef::new("declared", model),
            vectors: request
                .inputs
                .iter()
                .enumerate()
                .map(|(index, _)| EmbeddingVector {
                    index,
                    values: vec![index as f32],
                })
                .collect(),
            input_tokens: None,
        })
    }
}

#[capability_factory(pack = "declared-embedding", config = EmbeddingCapabilityConfig)]
fn declared_embedding(config: &EmbeddingCapabilityConfig) -> EmbeddingModelPlugin {
    let adapter: Arc<dyn EmbeddingModel> = Arc::new(DeclaredEmbeddingModel);
    EmbeddingModelPlugin::with_id(
        "declared-embedding.binding",
        EmbeddingModelService::new(ModelRef::new(&config.connection, &config.model), adapter)
            .protocol("test"),
    )
}

#[derive(Clone)]
struct ToolCapabilityConfig {
    id: String,
    call_name: String,
    description: String,
}

struct DeclaredTool;

#[async_trait]
impl Tool for DeclaredTool {
    async fn call(&self, _: serde_json::Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("declared tool"))
    }
}

#[capability_factory(pack = "declared-tool", config = ToolCapabilityConfig)]
fn declared_tool(config: &ToolCapabilityConfig) -> ToolBindingPlugin {
    ToolBindingPlugin::new(
        "declared-tool.binding",
        ToolBinding::new(
            ToolDescriptor::new(
                &config.id,
                &config.call_name,
                &config.description,
                json!({"type": "object"}),
            ),
            DeclaredTool,
        )
        .parallel_safe(),
    )
}

const EMBEDDED_SKILL_FILES: &[CapabilitySkillFile] = &[
    CapabilitySkillFile {
        path: "embedded-review/SKILL.md",
        contents: b"---\nname: embedded-review\ndescription: Review embedded capability package resources.\n---\nUse the checklist before answering.\n",
    },
    CapabilitySkillFile {
        path: "embedded-review/references/checklist.md",
        contents: b"- Verify the package source.\n",
    },
];

impl RecordedPlugin {
    fn new(id: &'static str, state: Arc<fixture::FixtureState>) -> Self {
        Self { id, state }
    }
}

#[async_trait]
impl Plugin for RecordedPlugin {
    fn id(&self) -> &str {
        self.id
    }

    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        self.state.record(format!("install:{}", self.id));
        let id = self.id;
        let state = self.state.clone();
        context.on_shutdown(move || async move {
            state.record(format!("close:{id}"));
            Ok(())
        });
        Ok(())
    }
}

fn capability_error(error: AgentError, expected: &str) {
    let message = error.to_string();
    assert!(
        matches!(error, AgentError::Capability(_)),
        "expected a capability error, got {message}"
    );
    assert!(
        message.contains(expected),
        "expected `{expected}` in capability error `{message}`"
    );
}

#[tokio::test]
async fn third_party_extension_points_are_available_to_consumers_in_ready() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let agent = builder(UnusedLoop)
        .plugin(PaymentRuleConsumer(observed.clone()))
        .capability_pack(CapabilityPackBuilder::new("custom-extension", ()))
        .build()
        .await
        .unwrap();

    assert_eq!(
        *observed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        ["external-payment-rule"]
    );
    assert!(agent.close().await.is_success());
}

#[tokio::test]
async fn built_in_agent_loop_can_be_declared_directly_by_a_capability_package() {
    let agent = AgentBuilder::new()
        .default_models(false)
        .plugin(ConversationStorePlugin::new(
            TestConversationStore::default(),
        ))
        .capability_pack(CapabilityPackBuilder::new("declared-agent-loop", ()))
        .build()
        .await
        .unwrap();

    let result = agent
        .run(RunRequest::new("declared-agent-loop", "run"))
        .await
        .unwrap();
    assert!(matches!(result.status, RunStatus::Completed));
    assert!(agent.close().await.is_success());
}

#[tokio::test]
async fn built_in_chat_model_can_be_declared_directly_with_its_default_connection() {
    let agent = builder(UnusedLoop)
        .capability_pack(CapabilityPackBuilder::new("declared-chat-model", ()))
        .build()
        .await
        .unwrap();

    assert_eq!(
        agent.models(ModelOperation::Chat),
        vec![ModelRef::new("default", "default")]
    );
    assert!(agent.close().await.is_success());
}

#[tokio::test]
async fn aliased_built_in_context_provider_uses_the_framework_service_adapter() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let agent = builder(InspectContexts(observed.clone()))
        .capability_pack(CapabilityPackBuilder::new("aliased-context-provider", ()))
        .build()
        .await
        .unwrap();

    agent
        .run(RunRequest::new("aliased-context-provider", "inspect"))
        .await
        .unwrap();
    assert!(
        observed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .any(|message| message == "aliased context provider")
    );
    assert!(agent.close().await.is_success());
}

#[tokio::test]
async fn a_shared_skills_plugin_creates_a_fresh_runtime_for_each_agent() {
    let skills = Arc::new(SkillsPlugin::sources_only());

    let first = builder(UnusedLoop)
        .plugin_arc(skills.clone())
        .build()
        .await
        .unwrap();
    assert!(first.close().await.is_success());

    let second = builder(UnusedLoop)
        .plugin_arc(skills)
        .build()
        .await
        .unwrap();
    assert!(second.close().await.is_success());
}

#[tokio::test]
async fn single_third_party_extension_is_visible_once_and_rejects_duplicates() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let agent = builder(UnusedLoop)
        .plugin(CheckoutAuthorityConsumer(observed.clone()))
        .capability_pack(CapabilityPackBuilder::new("single-extension.primary", ()))
        .build()
        .await
        .unwrap();

    assert_eq!(
        *observed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        ["primary-checkout-authority"]
    );
    assert!(agent.close().await.is_success());

    let error = builder(UnusedLoop)
        .capability_pack(CapabilityPackBuilder::new("single-extension.primary", ()))
        .capability_pack(CapabilityPackBuilder::new("single-extension.secondary", ()))
        .build()
        .await
        .err()
        .expect("two implementations of a single extension point must fail");
    assert!(
        error.to_string().contains("single extension")
            && error.to_string().contains("CheckoutAuthority"),
        "unexpected singleton extension error: {error}"
    );
}

#[tokio::test]
async fn context_storage_interfaces_can_be_declared_as_capabilities() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let agent = builder(UnusedLoop)
        .plugin(ContextStorageConsumer(observed.clone()))
        .capability_pack(CapabilityPackBuilder::new(
            "declared-context-summary-store",
            (),
        ))
        .capability_pack(CapabilityPackBuilder::new(
            "declared-context-artifact-store",
            (),
        ))
        .build()
        .await
        .unwrap();

    assert_eq!(
        *observed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        ["context-storage-ready"]
    );
    assert!(agent.close().await.is_success());
}

#[tokio::test]
async fn declared_skill_provider_creates_the_shared_skills_runtime_without_filesystem_sources() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let agent = builder(InspectContexts(observed.clone()))
        .capability_pack(CapabilityPackBuilder::new("declared-skill-provider", ()))
        .build()
        .await
        .unwrap();

    agent
        .run(RunRequest::new("declared-skills", "inspect"))
        .await
        .unwrap();
    assert!(
        observed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .any(|message| message.contains("declared-review")),
        "the capability SkillProvider must share the same Skills endpoint"
    );
    assert!(agent.close().await.is_success());
}

#[tokio::test]
async fn only_skills_filters_the_merged_capability_provider_catalog() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let agent = builder(InspectContexts(observed.clone()))
        .plugin(SkillsPlugin::sources_only().only_skills(["declared-review"]))
        .capability_pack(CapabilityPackBuilder::new("declared-skill-provider", ()))
        .build()
        .await
        .unwrap();

    agent
        .run(RunRequest::new("declared-skills-filtered", "inspect"))
        .await
        .unwrap();
    let contexts = observed
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .join("\n");
    assert!(contexts.contains("declared-review"));
    assert!(!contexts.contains("declared-hidden"));
    assert!(agent.close().await.is_success());
}

#[tokio::test]
async fn capability_factory_skills_runtime_aggregates_package_sources_once() {
    let root = std::env::temp_dir().join(format!(
        "eliteforge-factory-skills-runtime-{}",
        RunId::new_v4()
    ));
    let skill = root.join("factory-skill");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: factory-skill\ndescription: A factory-owned Skills runtime.\n---\nUse this skill.\n",
    )
    .unwrap();

    let observed = Arc::new(Mutex::new(Vec::new()));
    let agent = builder(InspectServiceCounts(observed.clone()))
        .capability_pack(
            CapabilityPackBuilder::new("factory-skills-runtime", ()).skills_directory(&root),
        )
        .build()
        .await
        .unwrap();

    agent
        .run(RunRequest::new("factory-skills-runtime", "inspect"))
        .await
        .unwrap();
    assert_eq!(
        *observed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        [(1, 1)],
        "a configured SkillsPlugin must own the one aggregate endpoint"
    );
    assert!(agent.close().await.is_success());
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn duplicate_names_across_selected_skill_providers_are_rejected() {
    let agent = builder(InspectContexts(Arc::new(Mutex::new(Vec::new()))))
        .capability_pack(CapabilityPackBuilder::new("declared-skill-provider", ()))
        .capability_pack(CapabilityPackBuilder::new(
            "duplicate-declared-skill-provider",
            (),
        ))
        .build()
        .await
        .unwrap();

    let error = agent
        .run(RunRequest::new("duplicate-declared-skills", "inspect"))
        .await
        .expect_err("duplicate skill names must fail before a skill is exposed");
    assert!(
        error.to_string().contains("provided by both"),
        "unexpected duplicate skill error: {error}"
    );
    assert!(agent.close().await.is_success());
}

#[tokio::test]
async fn factory_capabilities_can_register_non_trait_plugin_configuration() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let agent = builder(UnusedLoop)
        .capability_pack(CapabilityPackBuilder::new(
            "factory-capability",
            FactoryPluginConfig(events.clone()),
        ))
        .build()
        .await
        .unwrap();

    assert_eq!(
        *events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        ["factory-plugin-installed", "factory-plugin-ready"]
    );
    assert!(agent.close().await.is_success());
}

#[tokio::test]
async fn ready_failure_from_a_capability_factory_is_forwarded_and_rolled_back() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let error = builder(UnusedLoop)
        .capability_pack(CapabilityPackBuilder::new(
            "ready-failure",
            ReadyFailureConfig(events.clone()),
        ))
        .build()
        .await
        .err()
        .expect("the capability ready failure must fail Agent construction");

    assert!(matches!(
        error,
        AgentError::Plugin(PluginError::ReadyFailed { plugin, .. }) if plugin == "ready-failure.plugin"
    ));
    assert_eq!(
        *events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        ["install", "ready", "cleanup"]
    );
}

#[tokio::test]
async fn factory_capability_registers_a_configured_model_operation_binding() {
    let config = EmbeddingCapabilityConfig {
        connection: "declared".into(),
        model: "embedding-v1".into(),
    };
    let model = ModelRef::new(&config.connection, &config.model);
    let agent = builder(UnusedLoop)
        .plugin(ChatModelPlugin::new(DeclaredChatModel))
        .capability_pack(CapabilityPackBuilder::new("declared-embedding", config))
        .build()
        .await
        .unwrap();

    assert_eq!(agent.models(ModelOperation::Embedding), vec![model.clone()]);
    let mut request = EmbeddingRequest::texts(["one", "two"]);
    request.choice = ModelChoice::model(model);
    let response = agent.embed(request).await.unwrap();
    assert_eq!(response.model, ModelRef::new("declared", "embedding-v1"));
    assert_eq!(response.vectors.len(), 2);
    assert!(agent.close().await.is_success());
}

#[tokio::test]
async fn factory_capability_registers_one_explicitly_described_tool() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let config = ToolCapabilityConfig {
        id: "declared-tool.echo".into(),
        call_name: "declared_echo".into(),
        description: "Echoes a capability-pack declaration.".into(),
    };
    let agent = builder(InspectTools(observed.clone()))
        .capability_pack(CapabilityPackBuilder::new("declared-tool", config))
        .build()
        .await
        .unwrap();

    agent
        .run(RunRequest::new("declared-tool", "inspect"))
        .await
        .unwrap();
    let exposes_declared_tool = {
        let descriptors = observed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        descriptors.iter().any(|descriptor| {
            descriptor.id == "declared-tool.echo"
                && descriptor.call_name == "declared_echo"
                && descriptor.description == "Echoes a capability-pack declaration."
        })
    };
    assert!(exposes_declared_tool);
    assert!(agent.close().await.is_success());
}

#[tokio::test]
async fn selected_package_embedded_skills_are_prepared_and_exposed_once() {
    let root =
        std::env::temp_dir().join(format!("eliteforge-capability-skills-{}", RunId::new_v4()));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let pack =
        CapabilityPackBuilder::new("embedded-skills", ()).skills(CapabilitySkillSource::embedded(
            EmbeddedCapabilitySkills::new(EMBEDDED_SKILL_FILES, "embedded-review-v1"),
        ));
    let agent = builder(InspectContexts(observed.clone()))
        .capability_skills_root(&root)
        .capability_pack(pack)
        .build()
        .await
        .unwrap();

    agent
        .run(RunRequest::new("embedded-skills", "inspect"))
        .await
        .unwrap();
    let contexts = observed
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    assert_eq!(
        contexts
            .iter()
            .filter(|context| context.contains("embedded-review"))
            .count(),
        1,
        "selected skills should share one Skills runtime"
    );
    assert!(root.join("capability-packs").is_dir());
    assert!(agent.close().await.is_success());
    assert!(
        root.join("capability-packs").is_dir(),
        "the application-managed resource cache must outlive an Agent"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn embedded_package_skills_require_an_application_managed_resource_root() {
    let error = builder(UnusedLoop)
        .capability_pack(
            CapabilityPackBuilder::new("embedded-skills-without-root", ()).skills(
                CapabilitySkillSource::embedded(EmbeddedCapabilitySkills::new(
                    EMBEDDED_SKILL_FILES,
                    "embedded-review-v1",
                )),
            ),
        )
        .build()
        .await
        .err()
        .expect("embedded package resources need a configured root");
    assert!(error.to_string().contains("capability_skills_root"));
}

#[tokio::test]
async fn selected_package_directory_skills_are_read_without_resource_preparation() {
    let root = std::env::temp_dir().join(format!(
        "eliteforge-capability-directory-skills-{}",
        RunId::new_v4()
    ));
    let skills = root.join("skills");
    let review = skills.join("directory-review");
    std::fs::create_dir_all(&review).unwrap();
    std::fs::write(
        review.join("SKILL.md"),
        "---\nname: directory-review\ndescription: Review a direct package directory.\n---\nUse the directory source.\n",
    )
    .unwrap();

    let observed = Arc::new(Mutex::new(Vec::new()));
    let agent = builder(InspectContexts(observed.clone()))
        .capability_pack(
            CapabilityPackBuilder::new("directory-skills", ()).skills_directory(&skills),
        )
        .build()
        .await
        .unwrap();

    agent
        .run(RunRequest::new("directory-skills", "inspect"))
        .await
        .unwrap();
    assert!(
        observed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .any(|context| context.contains("directory-review"))
    );
    assert!(agent.close().await.is_success());
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn capability_skills_macro_embeds_the_conventional_skills_directory() {
    let root = std::env::temp_dir().join(format!(
        "eliteforge-capability-skills-macro-{}",
        RunId::new_v4()
    ));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let agent = builder(InspectContexts(observed.clone()))
        .capability_skills_root(&root)
        .capability_pack(fixture::packaged_skills_pack())
        .build()
        .await
        .unwrap();

    agent
        .run(RunRequest::new("packaged-skills", "inspect"))
        .await
        .unwrap();
    assert!(
        observed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .any(|context| context.contains("fixture-review"))
    );
    assert!(agent.close().await.is_success());
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn selected_pack_uses_its_agent_configuration_and_leaves_linked_payment_inert() {
    let state = Arc::new(fixture::FixtureState::default());
    let observed = Arc::new(Mutex::new(Vec::new()));
    let agent = builder(InspectContexts(observed.clone()))
        .capability_pack(fixture::shop_pack(fixture::FixtureConfig::new(
            "shop-a",
            state.clone(),
        )))
        .build()
        .await
        .unwrap();

    assert_eq!(state.factory_count("shop"), 1);
    assert_eq!(
        fixture::payment_factory_count(),
        0,
        "a linked but unselected package must not create its Default capability"
    );
    agent
        .run(RunRequest::new("shop-a", "inspect"))
        .await
        .unwrap();
    assert!(
        observed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .any(|message| message == "shop:shop-a"),
        "the config passed to this Agent must reach the configured factory"
    );
    assert!(agent.close().await.is_success());
}

#[tokio::test]
async fn explicitly_included_child_package_is_activated_with_its_own_configuration() {
    let state = Arc::new(fixture::FixtureState::default());
    let agent = builder(UnusedLoop)
        .capability_pack(fixture::shop_pack_with_audit(
            fixture::FixtureConfig::new("shop", state.clone()),
            fixture::FixtureConfig::new("audit", state.clone()),
        ))
        .build()
        .await
        .unwrap();

    assert_eq!(state.factory_count("shop"), 1);
    assert_eq!(state.factory_count("audit"), 1);
    assert!(
        state.events().contains(&"factory:audit:audit".to_owned()),
        "a child package must be selected explicitly by its parent, not merely linked"
    );
    assert!(agent.close().await.is_success());
}

#[tokio::test]
async fn dependencies_have_a_stable_order_despite_inventory_collection_order() {
    for run in 0..3 {
        let state = Arc::new(fixture::FixtureState::default());
        let label = format!("run-{run}");
        let agent = builder(UnusedLoop)
            .capability_pack(fixture::ordering_pack(fixture::FixtureConfig::new(
                label.clone(),
                state.clone(),
            )))
            .build()
            .await
            .unwrap();
        let order: Vec<_> = state
            .events()
            .into_iter()
            .filter(|event| event.starts_with("factory:ordered-"))
            .collect();
        assert_eq!(
            order,
            vec![
                format!("factory:ordered-first:{label}"),
                format!("factory:ordered-second:{label}"),
            ]
        );
        assert!(agent.close().await.is_success());
    }
}

#[tokio::test]
async fn one_implementation_contributing_two_interfaces_is_created_once() {
    let state = Arc::new(fixture::FixtureState::default());
    let observed = Arc::new(Mutex::new(Vec::new()));
    let agent = builder(InspectContexts(observed.clone()))
        .capability_pack(fixture::shared_pack(fixture::FixtureConfig::new(
            "one-instance",
            state.clone(),
        )))
        .build()
        .await
        .unwrap();

    assert_eq!(
        state.factory_count("shared"),
        1,
        "ContextContributor and EventSink adapters must share their declared instance"
    );
    agent
        .run(RunRequest::new("shared", "inspect"))
        .await
        .unwrap();
    assert!(
        observed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .any(|message| message == "shared:one-instance")
    );
    assert!(agent.close().await.is_success());
}

#[tokio::test]
async fn two_agents_with_one_package_type_keep_configuration_and_lifecycle_isolated() {
    let state = Arc::new(fixture::FixtureState::default());
    let first_seen = Arc::new(Mutex::new(Vec::new()));
    let second_seen = Arc::new(Mutex::new(Vec::new()));
    let first = builder(InspectContexts(first_seen.clone()))
        .capability_pack(fixture::shop_pack(fixture::FixtureConfig::new(
            "first",
            state.clone(),
        )))
        .build()
        .await
        .unwrap();
    let second = builder(InspectContexts(second_seen.clone()))
        .capability_pack(fixture::shop_pack(fixture::FixtureConfig::new(
            "second",
            state.clone(),
        )))
        .build()
        .await
        .unwrap();

    assert_eq!(state.factory_count("shop"), 2);
    first
        .run(RunRequest::new("first", "inspect"))
        .await
        .unwrap();
    assert!(first.close().await.is_success());
    second
        .run(RunRequest::new("second", "inspect"))
        .await
        .unwrap();
    assert!(
        first_seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .any(|message| message == "shop:first")
    );
    assert!(
        second_seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .any(|message| message == "shop:second"),
        "closing the first Agent must not close the second package instance"
    );
    assert!(second.close().await.is_success());
}

#[tokio::test]
async fn duplicate_ids_missing_dependencies_cycles_and_plugin_id_conflicts_fail_before_factories() {
    let duplicate_state = Arc::new(fixture::FixtureState::default());
    let error = builder(UnusedLoop)
        .capability_pack(fixture::duplicate_pack(fixture::FixtureConfig::new(
            "duplicate",
            duplicate_state.clone(),
        )))
        .build()
        .await
        .err()
        .expect("duplicate capability IDs must be rejected");
    capability_error(error, "duplicates.same-id");
    assert_eq!(duplicate_state.factory_count("duplicate-one"), 0);
    assert_eq!(duplicate_state.factory_count("duplicate-two"), 0);

    let missing_state = Arc::new(fixture::FixtureState::default());
    let error = builder(UnusedLoop)
        .capability_pack(fixture::missing_dependency_pack(
            fixture::FixtureConfig::new("missing", missing_state.clone()),
        ))
        .build()
        .await
        .err()
        .expect("missing dependencies must be rejected");
    capability_error(error, "not-declared");
    assert_eq!(missing_state.factory_count("missing-dependency"), 0);

    let cycle_state = Arc::new(fixture::FixtureState::default());
    let error = builder(UnusedLoop)
        .capability_pack(fixture::cycle_pack(fixture::FixtureConfig::new(
            "cycle",
            cycle_state.clone(),
        )))
        .build()
        .await
        .err()
        .expect("cycles must be rejected");
    capability_error(error, "cycle");
    assert_eq!(cycle_state.factory_count("cycle-a"), 0);
    assert_eq!(cycle_state.factory_count("cycle-b"), 0);

    let conflict_state = Arc::new(fixture::FixtureState::default());
    let error = builder(UnusedLoop)
        .plugin(RecordedPlugin::new(
            "plugin.conflict",
            conflict_state.clone(),
        ))
        .capability_pack(fixture::plugin_conflict_pack(fixture::FixtureConfig::new(
            "conflict",
            conflict_state.clone(),
        )))
        .build()
        .await
        .err()
        .expect("a capability may not shadow an ordinary plugin ID");
    capability_error(error, "plugin.conflict");
    assert_eq!(conflict_state.factory_count("plugin-conflict"), 0);
    assert!(
        conflict_state.events().is_empty(),
        "the ordinary plugin must not install after a preflight conflict"
    );
}

#[tokio::test]
async fn defaults_plugin_id_is_reserved_but_not_a_capability_dependency() {
    let conflict_state = Arc::new(fixture::FixtureState::default());
    let error = builder(UnusedLoop)
        .capability_pack(fixture::reserved_default_id_pack(
            fixture::FixtureConfig::new("reserved-id", conflict_state.clone()),
        ))
        .build()
        .await
        .err()
        .expect("a capability may not claim the defaults plugin ID");
    assert!(matches!(
        error,
        AgentError::Capability(CapabilityError::PluginIdConflict { capability })
            if capability == "builtin.framework-defaults"
    ));
    assert_eq!(conflict_state.factory_count("reserved-default-id"), 0);

    let dependency_state = Arc::new(fixture::FixtureState::default());
    let error = builder(UnusedLoop)
        .capability_pack(fixture::reserved_default_dependency_pack(
            fixture::FixtureConfig::new("reserved-dependency", dependency_state.clone()),
        ))
        .build()
        .await
        .err()
        .expect("a capability may not depend on the later defaults plugin");
    assert!(matches!(
        error,
        AgentError::Capability(CapabilityError::MissingDependency {
            capability,
            dependency,
        }) if capability == "reserved-default-dependency.context"
            && dependency == "builtin.framework-defaults"
    ));
    assert_eq!(
        dependency_state.factory_count("reserved-default-dependency"),
        0
    );
}

#[tokio::test]
async fn direct_plugins_and_pack_members_participate_in_the_capability_dependency_graph() {
    let state = Arc::new(fixture::FixtureState::default());
    let agent = builder(UnusedLoop)
        .capability_pack(
            test_pack().plugin(RecordedPlugin::new("legacy.dependency", state.clone())),
        )
        .plugin(RecordedPlugin::new("legacy.direct", state.clone()))
        .capability_pack(fixture::mixed_pack(fixture::FixtureConfig::new(
            "mixed",
            state.clone(),
        )))
        .build()
        .await
        .unwrap();

    let installed: Vec<_> = state
        .events()
        .into_iter()
        .filter(|event| event.starts_with("install:"))
        .collect();
    assert_eq!(
        installed,
        [
            "install:legacy.direct",
            "install:legacy.dependency",
            "install:mixed",
        ]
    );
    assert!(agent.close().await.is_success());
    let closed: Vec<_> = state
        .events()
        .into_iter()
        .filter(|event| event.starts_with("close:"))
        .collect();
    assert_eq!(
        closed,
        [
            "close:mixed",
            "close:legacy.dependency",
            "close:legacy.direct",
        ]
    );
}

#[tokio::test]
async fn capability_declared_plugin_failure_reuses_container_rollback_and_close() {
    let state = Arc::new(fixture::FixtureState::default());
    let error = builder(UnusedLoop)
        .plugin(RecordedPlugin::new("legacy.resource", state.clone()))
        .capability_pack(fixture::rollback_pack(fixture::FixtureConfig::new(
            "rollback",
            state.clone(),
        )))
        .build()
        .await
        .err()
        .expect("the capability plugin deliberately fails installation");

    assert!(matches!(
        error,
        AgentError::Plugin(PluginError::InstallationFailed { plugin, .. }) if plugin == "rollback.failure"
    ));
    assert_eq!(state.factory_count("rollback"), 1);
    let lifecycle: Vec<_> = state
        .events()
        .into_iter()
        .filter(|event| event.starts_with("install:") || event.starts_with("close:"))
        .collect();
    assert_eq!(
        lifecycle,
        [
            "install:legacy.resource",
            "install:rollback",
            "close:rollback",
            "close:legacy.resource",
        ]
    );
}

#[tokio::test]
async fn default_factory_is_shared_across_modules_and_different_impl_paths() {
    let agent = builder(UnusedLoop)
        .capability_pack(CapabilityPackBuilder::new("regression.default-shared", ()))
        .build()
        .await
        .unwrap();
    assert_eq!(
        fixture::regressions::DEFAULT_SHARED_CONSTRUCTIONS
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "one concrete type must have one default instance regardless of the impl module"
    );
    assert!(agent.close().await.is_success());
}

#[tokio::test]
async fn configured_factories_share_across_modules_and_aliases_with_implicit_or_explicit_keys() {
    for pack in ["regression.configured-shared", "regression.explicit-shared"] {
        let state = Arc::new(fixture::FixtureState::default());
        let observed = Arc::new(Mutex::new(Vec::new()));
        let agent = builder(InspectContexts(observed.clone()))
            .capability_pack(CapabilityPackBuilder::new(
                pack,
                fixture::FixtureConfig::new(pack, state.clone()),
            ))
            .build()
            .await
            .unwrap();
        assert_eq!(state.factory_count("cross-module"), 1);
        agent.run(RunRequest::new(pack, "inspect")).await.unwrap();
        assert_eq!(*observed.lock().unwrap(), [format!("cross-module:{pack}")]);
        assert!(
            state
                .events()
                .contains(&format!("event:cross-module:{pack}"))
        );
        assert!(agent.close().await.is_success());
    }
}

#[tokio::test]
async fn different_factory_functions_still_conflict_before_either_is_called() {
    let state = Arc::new(fixture::FixtureState::default());
    let error = builder(UnusedLoop)
        .capability_pack(CapabilityPackBuilder::new(
            "regression.factory-conflict",
            fixture::FixtureConfig::new("conflict", state.clone()),
        ))
        .build()
        .await
        .err()
        .expect("different factory functions cannot own one instance");
    assert!(matches!(
        error,
        AgentError::Capability(CapabilityError::InstanceFactoryConflict { .. })
    ));
    assert!(state.events().is_empty());
}

#[tokio::test]
async fn stacked_declarations_create_two_independent_instances_of_one_interface() {
    let state = Arc::new(fixture::FixtureState::default());
    let observed = Arc::new(Mutex::new(Vec::new()));
    let agent = builder(InspectContexts(observed.clone()))
        .capability_pack(CapabilityPackBuilder::new(
            "regression.repeated",
            fixture::FixtureConfig::new("repeated", state.clone()),
        ))
        .build()
        .await
        .unwrap();
    assert_eq!(state.factory_count("repeated"), 2);
    agent
        .run(RunRequest::new("repeated", "inspect"))
        .await
        .unwrap();
    assert_eq!(*observed.lock().unwrap(), ["repeated", "repeated"]);
    assert!(agent.close().await.is_success());
}

struct DeclaredDependencyPlugin {
    dependency: &'static str,
    state: Arc<fixture::FixtureState>,
}

#[async_trait]
impl Plugin for DeclaredDependencyPlugin {
    fn id(&self) -> &str {
        "regression.manual"
    }
    fn dependencies(&self) -> Vec<String> {
        vec![self.dependency.into()]
    }
    async fn install(&self, _: &mut PluginContext<'_>) -> Result<(), PluginError> {
        self.state.record("install:regression.manual");
        Ok(())
    }
}

#[tokio::test]
async fn mixed_plugin_cycle_and_missing_plugin_dependencies_fail_before_any_factory() {
    for dependency in ["regression.mixed.a", "regression.missing"] {
        let state = Arc::new(fixture::FixtureState::default());
        let error = builder(UnusedLoop)
            .plugin(DeclaredDependencyPlugin {
                dependency,
                state: state.clone(),
            })
            .capability_pack(CapabilityPackBuilder::new(
                "regression.mixed",
                fixture::FixtureConfig::new("mixed", state.clone()),
            ))
            .build()
            .await
            .err()
            .expect("invalid combined graph must fail preflight");
        match dependency {
            "regression.mixed.a" => assert!(matches!(
                error,
                AgentError::Plugin(PluginError::DependencyCycle { .. })
            )),
            _ => assert!(matches!(
                error,
                AgentError::Plugin(PluginError::MissingDependency { .. })
            )),
        }
        assert!(
            state.events().is_empty(),
            "neither capability factories nor ordinary plugins may run"
        );
    }
}

#[tokio::test]
async fn factory_order_follows_transitive_dependencies_through_ordinary_plugins() {
    let state = Arc::new(fixture::FixtureState::default());
    let agent = builder(UnusedLoop)
        .plugin(DeclaredDependencyPlugin {
            dependency: "regression.mixed.z",
            state: state.clone(),
        })
        .capability_pack(CapabilityPackBuilder::new(
            "regression.mixed",
            fixture::FixtureConfig::new("mixed", state.clone()),
        ))
        .build()
        .await
        .unwrap();
    assert_eq!(
        state
            .events()
            .into_iter()
            .filter(|event| event.starts_with("factory:"))
            .collect::<Vec<_>>(),
        [
            "factory:mixed-producer:mixed",
            "factory:mixed-consumer:mixed"
        ],
        "z -> ordinary plugin -> a must override alphabetical capability order"
    );
    assert!(agent.close().await.is_success());
}

fn test_pack() -> CapabilityPackBuilder<()> {
    static ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    CapabilityPackBuilder::new(
        format!(
            "test.pack.{}",
            ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ),
        (),
    )
}
