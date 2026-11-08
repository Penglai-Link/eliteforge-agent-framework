# 装配与配置

本章沿用入门程序的 Cargo 依赖。代码中的 `builder` 是尚未 `.build()` 的 `FrameworkBuilder`，`framework` 是构建完成的实例。

## 官方包提供什么

```rust
use eliteforge_agent_official_capability_pack::EliteForgeOfficialCapabilityPack;
let official = EliteForgeOfficialCapabilityPack::new("agent-data/state.sqlite", "agent-data/artifacts");
```

| 默认安装 | 实际效果 | 如何改配置 |
| --- | --- | --- |
| Genai | 注册模型连接；需要模型配置 | `.model(configured_plugin)` |
| LocalStorage | SQLite 状态、会话、交互、记忆、摘要及工件 | `.storage(local_storage_plugin)` |
| Human | `ask_user` 候选工具 | `.human_interaction(false)` 关闭 |
| Agents | 子 Agent 和独立会话工具候选；需宿主提供解析/调度服务 | `.agent_tools(false)` 关闭 |
| Skills | 扫描技能来源并提供目录、加载工具 | `.skills(configured_plugin)` 或 `.without_skills()` |
| AG-UI | 输出适配器，可生成 SSE 帧 | `.agui(configured_plugin)` 或 `.without_agui()` |
| MCP | 默认没有服务器、没有远端工具 | `.mcp(configured_plugin)` 可加多个 |
| OTel | 使用宿主全局 provider | `.telemetry(configured_plugin)` |
| Chunking | 内置切片策略 | `.chunking(strategies)` 或 `.without_chunking()` |

本地文件读写、系统命令不默认启用。Web、Media、Discovery 除了配置外，还需在官方包依赖上启用同名 Cargo feature。选择官方包不代表 Agent 自动获得全部工具。

替换模型应写 `official.model(my_model_plugin)`，不要在官方包默认模型之外再安装同 ID 插件。`without_model()` 只关闭包内模型；若要完全关闭 Framework 的默认模型装配，同时设置 `FrameworkBuilder::default_models(false)`。

## 两个 Agent，共享资源，选择不同能力

下面假设已安装 ID 为 `shop.orders` 的工具 Provider、`shop.instructions` 的上下文来源（订单模板提供它们），以及入门程序的模型和存储：

```rust
use eliteforge_agent_framework::{AgentSelection, RunRequest};

let chat = framework.agent(AgentSelection::default()).build().await?;
let support = framework.agent(AgentSelection {
    tool_providers: Some(vec!["shop.orders".into()]),
    tools: Some(vec!["shop.orders.lookup".into()]),
    context_sources: Some(vec!["shop.instructions".into()]),
    ..Default::default()
}).build().await?;

let answer = support.run(RunRequest::new("customer-42-chat-1", "查一下 A100")).await?;
println!("{}", answer.output);
chat.close().await;
// support 仍可运行；数据库和模型插件属于 framework。
```

同一 Framework 的存储服务共享。不同 Agent 对象不是租户隔离边界；宿主必须校验用户能否访问 session，并为工具绑定对应账号的数据范围。目录、凭据或数据库必须完全隔离时，创建不同 Framework。

## AgentSelection 字段如何填写

| 字段 | 填写值 | 常见错误 |
| --- | --- | --- |
| `model` | `ModelRef::new(connection_id, model_id)` | 把插件 ID 当 connection_id |
| `model_strategy` | 注册 `ModelStrategyPlugin` 的 ID，如 `model.strategy` | 安装策略后未选中 |
| `agent_loop` | 提供 LoopService 的插件 ID | 自定义 Loop 安装后仍期望默认选择 |
| `tool_providers` | Provider 所属插件 ID 列表 | 填入 pack ID 或 `lookup_order` |
| `tools` | 所选 Provider 内工具的内部 ID | 填入模型调用名 |
| `tool_policies` | 工具策略插件 ID 列表 | 用空列表意外移除策略 |
| `context_sources` | ContextContributor 所属插件 ID 列表 | 填入 Data block_id 而非插件 ID |
| `skills` | frontmatter 中的技能 name | 填文件路径 |
| `outputs` | 输出工厂所属插件 ID 列表 | 以为过滤输出就会停止运行 |
| `context_assembler/budget/reducer` | 对应候选服务的插件 ID | 自定义组装器与另一套预算配置混装 |

`None` 与 `Some(vec![])` 不同。`None` 通常表示继承 Framework 默认；默认 `tool_providers`、`skills`、`context_sources` 均为空。`tools = None` 不额外缩小已选 Provider 的工具；`Some(vec![])` 则不允许其中任何工具。策略与输出默认保留已安装服务；需要筛选时显式填写列表。工具策略不通过 `.defaults(...)` 自动继承选择字段，需在 Agent 上明确选择或保留默认全部策略。

可为多个 Agent 设置公共默认：

```rust
use eliteforge_agent_framework::AgentSelection;
let builder = builder.defaults(AgentSelection {
    context_sources: Some(vec!["shop.instructions".into()]),
    ..Default::default()
});
```

## 已有配置如何切换

只变更已安装候选的选择，等待 Agent 空闲后重新配置：

```rust
let next = framework.agent(AgentSelection {
    tool_providers: Some(vec!["shop.orders".into()]),
    context_sources: Some(vec!["shop.instructions".into()]),
    ..Default::default()
});
support.reconfigure(next).await?;
```

失败时保留原配置。新插件集合不能通过选择字段安装；要更换数据库插件或增加连接生命周期，先停止接受新任务、排空旧环境，再建立新的 Framework。不要边跑边覆盖旧插件资源。

## 不使用官方组合包

只需要模型和 SQLite 时，可以手工装配。增加如下依赖（其余版本配置同入门程序）：

```toml
eliteforge-agent-official-plugin-model-genai = { git = "https://gitee.com/person-ai/eliteforge-agent-framework.git", tag = "1.0.0" }
eliteforge-agent-official-plugin-storage-local = { git = "https://gitee.com/person-ai/eliteforge-agent-framework.git", tag = "1.0.0" }
```

```rust
use eliteforge_agent_framework::FrameworkBuilder;
use eliteforge_agent_official_plugin_storage_local::LocalStoragePlugin;

// model_plugin 为按“模型接入”章构造的 GenaiPlugin；附件目录需事先创建并保护。
let framework = FrameworkBuilder::new()
    .default_models(false)
    .plugin(model_plugin)
    .plugin(LocalStoragePlugin::new("agent-data/state.sqlite", "agent-data/artifacts"))
    .build().await?;
```

这条路径没有 Skills、问答或 AG-UI；需要哪个再加入哪个插件。缺存储会报配置错误，不会自动退回内存。
