//! Persistent history, summaries, independent context blocks and artifact reads.
//! Running calls the configured API. Re-run against the same MEMORY_DIR to continue.
use eliteforge_agent_context::*;
use eliteforge_agent_framework::*;
use eliteforge_agent_memory::*;
use eliteforge_agent_official_plugin_model_genai::{AdapterKind, AuthData, GenaiChatAdapter};
use eliteforge_agent_official_plugin_storage_local::{
    FileArtifactLimits, FileArtifactStore, MemoryStoreOptions, SqliteConversationOptions,
    SqliteConversationStore, SqliteLongTermMemory, SqliteOptions, SqliteStateStore,
    SqliteSummaryStore, SummaryStoreOptions,
};
use std::sync::Arc;

// The host supplies configuration; dependency assembly and resource ownership
// stay inside the plugin. This example retains its original database layout.
struct MemoryContextPlugin {
    directory: std::path::PathBuf,
    model: String,
}
impl MemoryContextPlugin {
    async fn install_components(
        &self,
        context: &mut PluginContext<'_>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let directory = &self.directory;
        std::fs::create_dir_all(directory.join("artifacts"))?;
        let conversations = Arc::new(
            SqliteConversationStore::open(
                directory.join("conversations.db"),
                SqliteConversationOptions::default(),
            )
            .await?,
        );
        let close = conversations.clone();
        context.on_shutdown(move || async move { close.close().await.map_err(|e| e.to_string()) });
        let summaries = Arc::new(
            SqliteSummaryStore::open(
                directory.join("summaries.db"),
                SummaryStoreOptions::default(),
            )
            .await?,
        );
        let close = summaries.clone();
        context.on_shutdown(move || async move { close.close().await.map_err(|e| e.to_string()) });
        let memories = Arc::new(
            SqliteLongTermMemory::open(
                directory.join("memories.db"),
                MemoryStoreOptions::default(),
            )
            .await?,
        );
        let close = memories.clone();
        context.on_shutdown(move || async move { close.close().await.map_err(|e| e.to_string()) });

        let state = Arc::new(
            SqliteStateStore::open(directory.join("execution.sqlite"), SqliteOptions::default())
                .await?,
        );
        let close = state.clone();
        context.on_shutdown(move || async move { close.close().await.map_err(|e| e.to_string()) });
        state.plugin().install(context).await?;
        let summary_store: Arc<dyn SummaryStore> = summaries.clone();
        context.provide(eliteforge_agent_context::SummaryStoreService(
            summary_store.clone(),
        ))?;
        context.contribute_extension(summary_store)?;
        let compressor = ModelCompressor::new(
            ModelRef::new("default", &self.model),
            CompressionOptions {
                max_summary_bytes: 1000,
                ..Default::default()
            },
        )?
        .with_store(summaries.clone(), "demo-history-v1")?;
        let artifacts = Arc::new(FileArtifactStore::open(
            directory.join("artifacts"),
            FileArtifactLimits {
                max_read_bytes: 1024,
                ..Default::default()
            },
        )?);
        let layout = ContextLayout(vec![
            ContextBlockConfig::new(INSTRUCTIONS_BLOCK, BlockBudget::tokens(1000)),
            ContextBlockConfig::new("memory", BlockBudget::tokens(1000)),
            ContextBlockConfig::new("task", BlockBudget::tokens(1000)),
            ContextBlockConfig::new(
                HISTORY_BLOCK,
                BlockBudget {
                    max_messages: Some(8),
                    ..BlockBudget::tokens(9000)
                },
            )
            .reduce_with(compressor),
        ]);
        let artifact_store: Arc<dyn ArtifactStore> = artifacts.clone();
        context.provide(eliteforge_agent_context::ArtifactStoreService(
            artifact_store.clone(),
        ))?;
        context.contribute_extension(artifact_store)?;
        ConversationStorePlugin::from_arc(conversations.clone())
            .install(context)
            .await?;
        LongTermMemoryPlugin::from_arc(memories.clone())
            .install(context)
            .await?;
        ContextContributorPlugin::new(
            "instructions",
            FixedInstructions::new(
                "fixed.instructions",
                "用中文简短回答，结合当前项目资料给出下一步建议。",
            ),
        )
        .install(context)
        .await?;
        ContextContributorPlugin::new(
            "memory-block",
            MemoryContributor::new("memory", "demo", memories.clone())?,
        )
        .install(context)
        .await?;
        ContextContributorPlugin::new(
            "task-block",
            FixedContextContributor(ContextBlock::data(
                "task",
                "目标：完成一个视频剪辑 Agent；待办：读取素材、生成剪辑方案、导出视频。",
            )),
        )
        .install(context)
        .await?;
        ContextLayoutPlugin(layout).install(context).await?;
        ToolProviderPlugin::new("memory-tools", MemoryTools::new(memories.clone(), "demo")?)
            .install(context)
            .await?;
        ToolProviderPlugin::new(
            "artifact-tools",
            StaticToolProvider::new(vec![
                ArtifactReadTool::new(artifacts.clone()).binding("artifact.read", "artifact_read"),
            ]),
        )
        .install(context)
        .await?;
        ToolMiddlewarePlugin::new(
            "offload",
            ToolOutputOffloader::new(artifacts, 4096, 256)?.excluding_tool("artifact.read"),
        )
        .install(context)
        .await?;
        Ok(())
    }
}
#[async_trait::async_trait]
impl Plugin for MemoryContextPlugin {
    fn id(&self) -> &str {
        "example.memory-context"
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        self.install_components(context)
            .await
            .map_err(|error| PluginError::message(error.to_string()))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let directory = std::path::PathBuf::from(
        std::env::var("MEMORY_DIR").unwrap_or_else(|_| "example-memory".into()),
    );
    let model = std::env::var("MODEL")?;
    let provider = Arc::new(GenaiChatAdapter::new(
        "default",
        AdapterKind::OpenAI,
        std::env::var("MODEL_ENDPOINT")?,
        AuthData::from_env("MODEL_API_KEY"),
    )?);
    let agent = AgentBuilder::new()
        .plugin(MemoryContextPlugin {
            directory,
            model: model.clone(),
        })
        .plugin(
            eliteforge_agent_official_plugin_model_genai::GenaiPlugin::from_adapter(
                provider,
                vec![ModelDescriptor::new(model)],
            ),
        )
        .build()
        .await?;
    let result = async {
        match agent.memory("demo", "project-language").await {
            Err(AgentError::LongTermMemory(MemoryError::NotFound)) => {
                agent
                    .save_memory(
                        "demo",
                        MemoryWrite {
                            id: "project-language".into(),
                            content: "示例项目使用 Rust 编写视频剪辑 Agent。".into(),
                            source: MemorySource {
                                id: "example:project-description".into(),
                                version: Some("1".into()),
                            },
                            expires_at_ms: None,
                        },
                        None,
                    )
                    .await?;
            }
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        // Keyword lookup uses all whitespace-separated terms. "Rust" matches the seed.
        let question = std::env::args().nth(1).unwrap_or_else(|| "Rust".into());
        let mut request = RunRequest::new("memory-demo", question);
        request.limits.max_context_tokens = 16_000;
        Ok::<_, Box<dyn std::error::Error>>(agent.run(request).await?)
    }
    .await;
    let shutdown = agent.close().await;
    println!("{}", result?.output);
    if !shutdown.is_success() {
        return Err(format!("shutdown failed: {shutdown:?}").into());
    }
    Ok(())
}
