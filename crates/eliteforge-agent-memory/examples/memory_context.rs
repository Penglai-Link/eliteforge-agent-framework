//! Persistent history, summaries, independent context blocks and artifact reads.
//! Running calls the configured API. Re-run against the same MEMORY_DIR to continue.
use eliteforge_agent_context::*;
use eliteforge_agent_framework::*;
use eliteforge_agent_memory::*;
use eliteforge_agent_official_plugin_model_genai::{AdapterKind, AuthData, GenaiChatAdapter};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let directory = std::path::PathBuf::from(
        std::env::var("MEMORY_DIR").unwrap_or_else(|_| "example-memory".into()),
    );
    std::fs::create_dir_all(directory.join("artifacts"))?;
    let conversations = Arc::new(
        SqliteConversationStore::open(
            directory.join("conversations.db"),
            SqliteConversationOptions::default(),
        )
        .await?,
    );
    let summaries = Arc::new(
        SqliteSummaryStore::open(
            directory.join("summaries.db"),
            SummaryStoreOptions::default(),
        )
        .await?,
    );
    let memories = Arc::new(
        SqliteLongTermMemory::open(directory.join("memories.db"), MemoryStoreOptions::default())
            .await?,
    );
    match memories.get("demo", "project-language").await {
        Err(MemoryError::NotFound) => {
            memories
                .save(
                    "demo",
                    &MemoryWrite {
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
    let model = std::env::var("MODEL")?;
    let provider = Arc::new(GenaiChatAdapter::new(
        "default",
        AdapterKind::OpenAI,
        std::env::var("MODEL_ENDPOINT")?,
        AuthData::from_env("MODEL_API_KEY"),
    )?);
    let compressor = ModelCompressor::new(
        ModelRef::new("default", &model),
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
    let agent = AgentBuilder::new()
        .plugin(
            std::sync::Arc::new(
                eliteforge_agent_state_sqlite::SqliteStateStore::open(
                    "execution.sqlite",
                    Default::default(),
                )
                .await?,
            )
            .plugin(),
        )
        .plugin(
            eliteforge_agent_official_plugin_model_genai::GenaiPlugin::from_adapter(
                provider,
                vec![ModelDescriptor::new(model)],
            ),
        )
        .plugin(ConversationStorePlugin::from_arc(conversations.clone()))
        .plugin(LongTermMemoryPlugin::from_arc(memories.clone()))
        .plugin(ContextPlugin::new(
            "instructions",
            FixedContext::new("用中文简短回答，结合当前项目资料给出下一步建议。"),
        ))
        .plugin(ContextContributorPlugin::new(
            "memory-block",
            MemoryContributor::new("memory", "demo", memories.clone())?,
        ))
        .plugin(ContextContributorPlugin::new(
            "task-block",
            FixedContextContributor(ContextBlock::data(
                "task",
                "目标：完成一个视频剪辑 Agent；待办：读取素材、生成剪辑方案、导出视频。",
            )),
        ))
        .plugin(ContextLayoutPlugin(layout))
        .plugin(ToolProviderPlugin::new(
            "memory-tools",
            MemoryTools::new(memories.clone(), "demo")?,
        ))
        .plugin(ToolProviderPlugin::new(
            "artifact-tools",
            StaticToolProvider::new(vec![
                ArtifactReadTool::new(artifacts.clone()).binding("artifact.read", "artifact_read"),
            ]),
        ))
        .plugin(ToolMiddlewarePlugin::new(
            "offload",
            ToolOutputOffloader::new(artifacts, 4096, 256)?.excluding_tool("artifact.read"),
        ))
        .build()
        .await?;
    // Keyword lookup uses all whitespace-separated terms. "Rust" matches the seed.
    let question = std::env::args().nth(1).unwrap_or_else(|| "Rust".into());
    let mut request = RunRequest::new("memory-demo", question);
    request.limits.max_context_tokens = 16_000;
    let result = agent.run(request).await;
    let shutdown = agent.close().await;
    let closed = (
        conversations.close().await,
        summaries.close().await,
        memories.close().await,
    );
    println!("{}", result?.output);
    if !shutdown.is_success() {
        return Err(format!("shutdown failed: {shutdown:?}").into());
    }
    closed.0?;
    closed.1?;
    closed.2?;
    Ok(())
}
