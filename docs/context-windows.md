# 会话存储与上下文窗口

`ConversationStore` 保存结构化会话消息，`ContextAssembler` 生成本次模型输入。
默认 ReAct、WorkflowLoop 和 DAG 内置 ReAct 都通过 ModelInvoker 使用组装器。
会话裁剪不会删除源消息。

## 会话记录

`ConversationStore.open(session_id)` 打开主分支；`open_branch(ConversationKey)` 打开指定分支。
`ConversationSession` 提供 `snapshot()`、`append()`、`append_at(revision, messages)` 和 `finish()`。
存储实现必须提供独占会话租约，保证版本检查和追加在同一租约内完成。

快照包含会话、分支、generation、revision 和原始消息。generation 标识这份历史的生命周期，
revision 是最后一条消息的序号；消息 ID 由会话、分支、generation 和序号组成。
会话显式过期后重建会生成新的 generation，旧消息 ID 不会被新消息复用。

消息保留模型结构化输出、工具调用参数、工具结果及协议续接数据。
指定输出 Schema 时，模型可只返回 structured；同时返回 JSON 正文与 structured 时，两者必须一致。
工具调用和结果成组追加，缺少对应结果的批次会被拒绝。
旧消息缺少新增加的 `structured`、`result_reference` 字段时按空值读取。

工作流通过 `TranscriptProjection` 提供检查点中的历史快照，历史写入仍由检查点提交负责。
节点快照使用运行、工作流和阶段组成的分支标识，不向独立会话存储重复写入。
普通 ReAct 也保存执行检查点，并按前缀将完整消息同步到 ConversationStore；
崩溃恢复时先补齐未同步的历史，再继续执行。

应用通过 Agent 查询和追加会话；存储连接由插件创建和关闭：

```rust
use eliteforge_agent_official_plugin_storage_sqlite::SqliteStoragePlugin;

let agent = AgentBuilder::new()
    .plugin(SqliteStoragePlugin::new("agent.sqlite"))
    .plugin(eliteforge_agent_official_plugin_model_genai::GenaiPlugin::from_adapter(provider, vec![ModelDescriptor::new(std::env::var("MODEL")?)]))
    .build().await?;
let key = ConversationKey::new("chat-1");
let history = agent.conversation(&key).await?;
agent.append_conversation(&key, history.revision, vec![Message::user("补充资料")]).await?;
```

默认不按空闲时间删除会话；显式设置非零 `session_ttl` 才启用过期清理，活跃租约不受影响。
超过条数或字节上限会拒绝追加，不会裁剪原始历史。租约续期保持 owner 不变；
进程退出后租约到期可重新获取，旧 owner 的写入会被拒绝。
应用调用 `agent.close().await`，插件负责关闭它创建的会话和执行存储连接。

自定义 DAG 节点可以显式记录交互：

```rust
let history = context.conversation_snapshot().await?;
context.save_with_conversation(
    serde_json::json!({"step": "inspected"}),
    history.revision,
    &messages, // 完整的调用与结果批次
).await?;
```

`save_with_conversation` 在同一检查点内保存节点状态和消息；仅追加消息可用 `append_conversation`。
快照可通过 `TranscriptProjection::from_snapshot` 交给统一模型调用器。
恢复沿用已保存的 generation；显式重试节点会创建新的调用身份和空节点历史。
纯计算节点无需调用这些接口，直接调用 ToolInvoker 也不会自动写入会话。


## 块预算与顺序

不配置动态块时，默认组装器只处理固定指令和会话历史。固定指令按实际大小计算，
剩余输入容量作为历史块的一次性额度，历史超额时保留最近完整轮次。

添加 Contributor 时，需要显式配置所有块的预算和顺序，包括内置指令块和历史块。

```rust
let layout = ContextLayout(vec![
    ContextBlockConfig::new(INSTRUCTIONS_BLOCK, BlockBudget::tokens(1000)),
    ContextBlockConfig::new("task", BlockBudget::tokens(2000)),
    ContextBlockConfig::new(HISTORY_BLOCK, BlockBudget::tokens(9000))
        .reduce_with(RecentConversation),
]);

let agent = AgentBuilder::new()
    .plugin(eliteforge_agent_official_plugin_storage_sqlite::SqliteStoragePlugin::new("agent.sqlite"))
    .plugin(eliteforge_agent_official_plugin_model_genai::GenaiPlugin::from_adapter(provider, vec![ModelDescriptor::new(std::env::var("MODEL")?)]))
    .plugin(ContextPlugin::new("prompt", FixedContext::new("用中文回答。")))
    .plugin(ContextContributorPlugin::new("task-data", FixedContextContributor(
        ContextBlock::data("task", "当前任务：设计视频剪辑 Agent。"),
    )))
    .plugin(ContextLayoutPlugin(layout))
    .build()
    .await?;
```

完整示例见 [context_blocks.rs](../crates/eliteforge-agent-official-plugin-model-genai/examples/context_blocks.rs)。
它使用现成实现，只有手工运行时才调用真实模型。

```text
指令块：1000 Token
  → task 块：2000 Token
  → 历史块：9000 Token
```

默认 `IndependentBlockBudget` 校验额度之和，不借用剩余额度，不按优先级保留，也不联合压缩。
`BlockBudget::ratio(2000)` 表示可分配输入容量的 20%；同时配置比例和绝对上限时取较小值。
工具定义和协议开销先从输入容量扣除。自定义布局超额会报错，不能假设各块实际较小就绕过配置检查。

`ContextBlockConfig::optional()` 允许该块失败或无法装入时省略，并记录原因。
省略后不把额度分给其他块。内置指令和历史块不能配置为可省略。

## 处理方式

| 实现 | 行为 |
|---|---|
| `KeepContext` | 原样保留，超出块额度时报错 |
| `RecentConversation` | 按完整轮次裁剪历史，保留固定指令和最新完整轮次 |
| `SelectContextEntries` | 按来源顺序选择能装入额度的数据条目 |
| `ModelCompressor` | 在历史块内生成摘要；辅助调用计入运行预算 |

Reducer 无法通过修改最新工具交互、丢失调用配对或把历史数据改为系统指令来通过校验。

## 摘要保存与复用

`SummaryStore` 保存派生摘要。框架提供 `SqliteSummaryStore`，
默认不保存；通过 `ModelCompressor::with_store` 开启。
依赖存储的组件在插件安装阶段组合，应用只配置插件：

```rust
use eliteforge_agent_context::{CompressionOptions, ModelCompressor};
use eliteforge_agent_official_plugin_storage_sqlite::{SqliteStoragePlugin, SummaryStoreService};
use async_trait::async_trait;
use std::sync::Arc;

struct SummaryCompressionPlugin { model: ModelRef }
#[async_trait]
impl Plugin for SummaryCompressionPlugin {
    fn id(&self) -> &str { "app.summary-compression" }
    fn dependencies(&self) -> Vec<String> { vec!["storage.sqlite".into()] }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let store = context.services().get::<SummaryStoreService>()
            .ok_or_else(|| PluginError::message("SummaryStore is required"))?;
        let compressor = ModelCompressor::new(self.model.clone(), CompressionOptions::default())
            .and_then(|compressor| compressor.with_store(store.0.clone(), "history-summary-v1"))
            .map_err(|error| PluginError::message(error.to_string()))?;
        ContextReducerPlugin::new(compressor).install(context).await
    }
}

let agent = AgentBuilder::new()
    .plugin(SqliteStoragePlugin::new("agent.sqlite"))
    .plugin(eliteforge_agent_official_plugin_model_genai::GenaiPlugin::from_adapter(provider, vec![
        ModelDescriptor::new("chat-model"), ModelDescriptor::new("summary-model"),
    ]))
    .plugin(ModelStrategyPlugin(Arc::new(FixedModelStrategy(ModelRef::new("default", "chat-model")))))
    .plugin(SummaryCompressionPlugin { model: ModelRef::new("default", "summary-model") })
    .build().await?;
let result = agent.run(RunRequest::new("chat-1", "继续上次的话题")).await;
let shutdown = agent.close().await;
println!("{}", result?.output);
assert!(shutdown.is_success());
```

显式配置布局时，在插件安装阶段把 `compressor` 传给历史块的 `.reduce_with(compressor)`，
不要再注册 `ContextReducerPlugin`。自定义组装器则在自身内部组合来源、布局和策略；
同时注册这些独立配置会在创建 Agent 时报告冲突，避免配置被静默忽略。

```text
读取会话快照
  → 按会话、分支、generation、块和策略版本查找摘要
  → 校验源消息范围和内容指纹
     ├─ 同一历史范围：复用摘要
     ├─ 追加了完整轮次：已有摘要 + 新轮次 → 更新摘要
     └─ 来源改变或配置改变：重新生成
  → 校验当前块额度
  → 按版本保存摘要，原始会话保持不变
```

摘要保存源 revision、覆盖消息范围和 SHA-256 指纹。Token 额度、模型或策略变化时使用不同的缓存标识；
若调整同名模型背后的实现或摘要含义，应用应更新 `history-summary-v1`。
并发更新使用不复用的版本标识，旧写入不能覆盖较新版本，删除后重建也不会接受旧版本写入。

摘要分批使用摘要连接自己的 Token 估算器，并扣除摘要模型的输出预留；辅助调用计入同一运行预算。
工具结果引用和最终结构化输出会作为历史数据参与摘要，供应商签名不会作为文本发给摘要模型。
生成后仍要校验实际块大小。摘要无法保证语义无损，原始历史和附件是追溯依据。

SQLite 只位于插件 crate 中，Framework 没有数据库依赖。
摘要缓存不能替代会话存储；重启复用还要求来源的会话标识、generation 和消息保持一致。
SqliteConversationStore 跨进程保留 generation，支持增量复用摘要。


## 模型容量与组装报告

`ModelDescriptor.context_window_tokens` 表示总窗口，需要扣除本次输出预留。
`max_input_tokens` 表示已经扣除输出预留的输入上限，两者同时配置时取更严格的结果。
`RunLimits.max_context_tokens` 是额外的运行级输入上限。
上下文超限返回 `ContextError::BudgetExceeded`；会话存储失败返回 `ConversationError`，两者分别报告。

选定模型后才组装窗口。路由到不同模型会重新计算，源会话快照保持不变。
中间件修改模型请求后，发送前重新校验结构、容量和工具授权。

`ContextAssembled` 事件提供各块顺序、来源版本、原始估算、分配额度、最终估算和省略原因，
不包含块正文。`ContextAssemblyFailed` 记录失败块的位置、额度和脱敏错误报告；
原始错误通过 `AgentError::ContextAssembly.source` 保留，`primary()` 可取得根错误。

`ModelDispatched` 记录中间件处理后真正发送的消息数、工具数、输入估算和输出额度。
诊断报告携带阶段、尝试编号和节点执行身份，可区分同一 DAG 内交错的模型调用。
被最终校验拒绝的请求没有发送事件；这些估算也不等于供应商账单用量。

TelemetryPlugin 将组装报告写到所在 run 或节点 span，把发送统计写到 model span，
摘要辅助调用标记为 `agent.model.summary=true`。默认不导出正文和来源标识。
`EventSink::on_diagnostic` 同步接收这三种元数据，便于在 span 结束前记录；实现必须快速、
不阻塞且不进行 I/O。普通事件仍通过有界异步队列消费，此入口不拦截或修改执行。

## 工具大结果

ToolOutputOffloader 共同检查 `content` 和 `structured` 的序列化大小。
超额时先保存完整 ToolOutput，再用预览与 `result_reference` 替换内联数据；
`structured` 置空，`is_error` 和 `effect` 保留。

Loop 将预览和引用写入消息，完整原文保存在 ArtifactStore。ArtifactReadTool 按范围读取原文，
读取片段作为新的工具结果进入历史。附件不会在组装窗口时自动展开。
附件内容是原始 ToolOutput 的 JSON；可从中恢复结构化结果和原始内容。

附件读取量必须受限，可通过 `excluding_tool` 排除读取工具，避免再次转存读取结果。
组合示例把每页限制为 1024 字节；读取结果、编码和消息开销都要能放入最新完整轮次的额度。
应用为每个授权范围提供对应的数据库或目录；仅创建多个实例、但共享同一个数据库文件，不能隔离访问。
默认组合插件使用 SqliteArtifactStore，FileArtifactStore 可单独选择；两者均不自动过期、不后台删除。查不到的附件返回 `ArtifactError::NotFound`，
表示该存储中不存在或已被宿主按保留策略删除，不能因此自动重跑原工具。

清理依据是源记录中的引用，而不是当前模型窗口：

1. 暂停该存储范围的新写入，并等待在途运行结束或进入可核对的停止状态。
2. 收集仍保留的会话、工作流检查点、长期记忆和业务记录中的附件引用。
3. 核对历史写入失败、取消后留下的已发布附件；恢复仍需使用的附件加入保留集合。
4. 仅对已确认不再引用、且达到应用保留期限的附件调用 `agent.execute(RemoveArtifact(id))`。

在途运行可能尚未把引用写入历史，所以不能边运行边根据一次引用快照清理。
首版提供原子发布、分页读取和显式删除；宿主负责完整引用清单、保留期限和清理调度。
未提交的原文宁可暂时多保留，不能依据上下文裁剪结果推断附件已无用。


## 长期记忆与短期记忆的组合

`LongTermMemory` 提供 `get`、`search`、`save` 和 `delete`。条目包含稳定 ID、内容、来源、
版本和可选有效期；namespace 是宿主确定的普通字符串，不引入 Tron 的业务枚举。

| 扩展接口 | 默认行为 | 框架提供的默认实现 |
|---|---|---|
| `LongTermMemory` | SQLite 插件注册；检索和写入按需启用 | `SqliteLongTermMemory` |
| `ContextContributor` | 不自动检索长期记忆 | `MemoryContributor` |
| `ToolProvider` | 不注册记忆工具 | `MemoryTools`：搜索、读取；可显式开启保存和删除 |

以下自定义组合插件复用已配置的长期记忆服务，不在应用代码中打开连接：

```rust
struct MemoryContextPlugin { namespace: String }
#[async_trait::async_trait]
impl Plugin for MemoryContextPlugin {
    fn id(&self) -> &str { "app.memory-context" }
    fn dependencies(&self) -> Vec<String> { vec!["storage.sqlite".into()] }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let memory = context.services().get::<LongTermMemoryService>()
            .ok_or_else(|| PluginError::message("LongTermMemory is required"))?;
        let contributor = MemoryContributor::new("memory", &self.namespace, memory.0.clone())
            .map_err(|error| PluginError::message(error.to_string()))?;
        let tools = MemoryTools::new(memory.0.clone(), &self.namespace)
            .map_err(|error| PluginError::message(error.to_string()))?;
        ContextContributorPlugin::new("memory-block", contributor).install(context).await?;
        ToolProviderPlugin::new("memory-tools", tools).install(context).await
    }
}

// builder 已配置 SqliteStoragePlugin，并在 ContextLayout 中设置 memory 块额度和位置。
let agent = builder
    .plugin(MemoryContextPlugin { namespace: "authorized-scope".into() })
    .build().await?;
let matches = agent.search_memory("authorized-scope", MemoryQuery::new("Rust")).await?;
```

默认检索按空白切分关键词，所有词都要匹配，按更新时间和 ID 排序；ASCII 不区分大小写，
其他字符精确匹配。它不做中文分词、向量检索或语义排序；需要这些能力时替换 LongTermMemory 后端。
检索条目按本块额度选择，带来源和版本作为数据加入上下文，不会升级为系统指令。

同一 namespace 中重复创建同一 ID 返回版本冲突；纠正与删除必须携带当前版本。
删除或过期后的重建生成新版本，旧版本不能覆盖新记录。不同 ID 的相似内容不会自动合并。
每次组装重新检索当前条目，没有长期记忆内容缓存；删除和纠正对后续检索立即生效。
已经写入会话的工具读取结果仍属于历史观察，删除长期记忆不会抹除原始对话。
外部来源更新后，宿主根据 source.version 更新相关条目；本轮不自动监控来源或提取事实。

短期记忆由原始会话、派生摘要、任务状态等来源共同组成。完整示例使用现成实现，组合：

```text
固定指令 → 检索记忆 → 任务目标与待办 → 摘要及最近会话
```

示例还配置了工具大结果转存和附件读取：
[memory_context.rs](../crates/eliteforge-agent-memory/examples/memory_context.rs)。

```sh
cargo run -p eliteforge-agent-memory --example memory_context -- Rust
```

设置 MODEL_ENDPOINT、MODEL_API_KEY、MODEL 后运行会调用真实模型。MEMORY_DIR 指定本地数据目录；
再次运行使用同一目录即可继续同一示例会话。流式接入继续使用[快速上手中的 start/take_events](quickstart.md#流式输出)，
持久化和分块配置不改变事件消费方式。
