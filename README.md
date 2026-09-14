# eliteforge-agent-framework

EliteForge Agent Framework 是一个可扩展的 Rust Agent 框架：Framework 负责装配、执行和生命周期管理，Plugin 提供模型、工具、存储和可观测能力，上层应用保留身份、权限和业务流程。

所有插件只配置到 `AgentBuilder`，应用通过构建后的 `agent` 实例操作：启动、控制、获取输出、调用模型、处理问答和关闭资源。
业务能力可以用 `#[extension_point]` 和 `#[capability(...)]` 声明，再由应用显式选择能力包自动装配。
能力包也可携带 Skills、工具、模型绑定和普通 Plugin；复杂资源初始化仍放在 Plugin 生命周期中。
依赖配置、完整入门示例和场景选择见[插件开发指南](docs/plugins.md)。

多个插件可以打成包，一次配置到 Agent：

```rust
use eliteforge_agent_framework::AgentBuilder;
use eliteforge_agent_official_plugin_pack::EliteForgeOfficialPluginPack;

let agent = AgentBuilder::new()
    .plugin_pack(EliteForgeOfficialPluginPack::new("agent.sqlite"))
    .build().await?;
```

官方包默认包含项目里的全部 8 个官方插件：Genai、SQLite、用户问答、Agent 协作工具、Skills、AGUI、MCP 和 OTel。
MCP 未配置服务器时工具集为空；OTel 在构建时读取上层应用的全局 provider。
`PluginPack` 继续用于官方和兼容的插件组合。

业务能力包使用另一套入口；只会装配显式选中的包及其 `.include(...)` 子包：

~~~rust
use eliteforge_agent_core::CapabilityPackBuilder;

let agent = AgentBuilder::new()
    .capability_pack(CapabilityPackBuilder::new("shop", shop_config))
    .build()
    .await?;
~~~

能力声明、配置工厂、共享实例和依赖规则见[能力包与自动装配](docs/capability-packs.md)。

```mermaid
flowchart LR
    Application[上层应用] --> Builder[AgentBuilder]
    Builder --> Agent[Agent]
    Agent --> Loop[ReAct / Workflow / DAG]
    Loop --> Model[模型]
    Loop --> Tool[工具与 MCP]
    Loop --> Store[SQLite / 会话 / 状态]
    Agent --> OTel[OpenTelemetry]
```

## 快速上手

### 1. 准备环境

需要 Rust/Cargo、Rust 工具链和 C 编译工具。先执行环境检查，再拉取锁定依赖：

```sh
make doctor
make install
```

### 2. 连接真实模型

Framework 默认启用 Genai。真实模型验证放在与 `crates/` 平级的 [integration-tests](integration-tests/README.md) 项目中，配置模型名和凭据后显式运行：

```sh
export MODEL="your-model"
export OPENAI_API_KEY="your-api-key"
cargo run -p eliteforge-agent-integration-tests --features live-model --bin default_model
```

代理服务可额外设置 `MODEL_ENDPOINT` 和 `MODEL_API_KEY`。完整代码和其他模型协议见[快速上手](docs/quickstart.md)。

## 支持的特性

| 分类 | 主流 Agent 常见特性 | 本框架状态 | 当前范围 |
| --- | --- | --- | --- |
| 运行 | ReAct 对话、流式输出与工具循环 | ✅ 已支持 | 文本与推理增量、工具错误回传、步骤和工具调用上限 |
| 运行 | 阶段工作流与 DAG 编排 | ✅ 已支持 | 固定阶段、并行节点、条件分支、汇合、恢复和自定义节点 |
| 模型 | 多供应商、多连接与模型选择 | ✅ 已支持 | 多连接选择；Chat Completions、Responses、原生 Gemini 与 Embedding |
| 内容 | 多模态输入与结构化输出 | ✅ 已支持 | 文本、图片等内容块、结构化回复和大型结果引用 |
| 工具 | 工具调用、Schema 校验与并发 | ✅ 已支持 | 调用前参数校验、显式安全的并发、撤销、取消和结果持久化 |
| 工具 | 目录范围文件、文本补丁、目录和代码搜索 | ✅ 已支持 | 可选文件系统能力包提供 `fs_*` 与 `search_*` 工具；所有路径限制在上层应用配置的根目录内 |
| 工具 | 系统命令执行（Linux、macOS、Windows） | ✅ 已支持 | 可选通用能力包提供 `system_exec`；能力包显式安装后可直接执行程序，也可按上层应用配置限制程序名；不隐式调用 Shell |
| 工具 | 编辑器工具 | 🧩 可扩展 | 由上层应用的 `ToolProvider` 或 MCP Server 提供 |
| 工具 | Web 搜索、抓取与浏览器自动化 | 🧩 可扩展 | 由上层应用工具或 MCP Server 接入 |
| 安全 | 工具权限、审批与策略 | 🧩 可扩展 | `ToolPolicy`、中间件和用户审批可组合；具体权限模型由上层应用定义 |
| 安全 | 沙箱执行 | 🏠 上层应用负责 | 框架传递取消、超时和策略；隔离运行环境由上层应用或工具实现 |
| 规划 | Plan Mode / 只读计划 | 🧩 可扩展 | 通过自定义 `AgentLoop`、`ToolPolicy` 和工具暴露策略实现 |
| 任务 | Todo、任务图与长期任务跟踪 | 🧩 可扩展 | 具备状态持久化、DAG 和恢复基础；未提供内置 Todo 或任务图产品模型 |
| 上下文 | 上下文窗口、压缩与 token 预算 | ✅ 已支持 | 按块组装、模型容量检查、历史裁剪和模型摘要 |
| 记忆 | 会话记忆、长期记忆与检索 | ✅ 已支持 | 可插拔接口、SQLite 后端、检索上下文和可选记忆工具 |
| 连续性 | 会话持久化、checkpoint 与恢复 | ✅ 已支持 | 原子 checkpoint、恢复身份、工具结果日志和中断后续跑 |
| 人机协作 | 用户问答与工具审批 | ✅ 已支持 | `AwaitInput`、持久化问题、允许/拒绝和跨进程恢复 |
| 运行控制 | 追加指令、取消与子执行额度 | ✅ 已支持 | Steering、取消、父子执行身份和预算隔离 |
| 集成 | MCP 工具与资源 | ✅ 已支持 | 官方 SDK transport、发现、分页、资源和取消传播 |
| 集成 | AG-UI 事件协议 | ✅ 已支持 | SSE、文本、推理、工具、状态、活动、交互恢复、子 Agent 和 Thread 生命周期事件 |
| 扩展 | Plugin、PluginPack 与能力包 | ✅ 已支持 | 声明式扩展点、全部 install 后 ready、回滚、异步关闭、Skills 聚合和 `#[capability]` / `#[capability_factory]` 自动装配 |
| 扩展 | Middleware、Hook 与自定义执行器 | ✅ 已支持 | Run、模型、工具、节点中间件；生命周期 Hook；自定义 `AgentLoop` |
| 扩展 | Skills 系统 | ✅ 已支持 | 官方文件系统插件兼容 Agent Skills；能力包可内嵌 `skills/`、引用目录或声明 Provider，统一聚合后按需读取资源 |
| 扩展 | 插件市场与在线安装 | — 未提供 | 当前提供 crate 与插件组合 API，未提供市场或安装器 |
| 多 Agent | 子 Agent / 专家 Agent | ✅ 已支持 | 官方 `agent_spawn` 工具；继承父运行取消、deadline、lineage、预算和深度 |
| 多 Agent | 独立 Threads 与会话协作 | ✅ 已支持 | 官方 `thread_create/send/answer/wait/read/list/close` 工具；默认进程内 bounded registry，跨进程持久化由上层 runtime 提供 |
| 多 Agent | 后台执行、队列与 Worktree 隔离 | 🏠 上层应用负责 | 需要部署队列、进程管理和 Git 工作树管理 |
| 多 Agent | Agent Team 协作 | 🧩 可扩展 | 可用 DAG、子执行、Threads 和事件接口编排；未提供固定团队决策协议 |
| 产品接口 | 终端 UI、REPL、Slash Commands、Headless HTTP | 🏠 上层应用负责 | Framework 提供运行时和事件；UI、CLI、HTTP 与命令路由由上层应用构建 |
| 产品接口 | 文件历史、Diff 与 Rewind | 🏠 上层应用负责 | 可作为工具或存储能力接入；框架不管理 Git 或目录文件 |
| 配置 | 项目级 / 用户级配置 Profile | 🏠 上层应用负责 | AgentBuilder 接收已解析配置；读取、合并和安全存储由上层应用负责 |
| 可观测 | OpenTelemetry、事件与指标 | ✅ 已支持 | 运行、节点、模型和工具 span；指标、错误事件和可注入 exporter |

各项的使用方式与约束见：[架构总览](docs/architecture.md)、
[上下文窗口](docs/context-windows.md)、[DAG 工作流](docs/dag-workflows.md)、
[交互与审批](docs/interactions.md)、[插件开发指南](docs/plugins.md)、
[能力包](docs/capability-packs.md)、[AG-UI](docs/agui.md) 和
[可观测](docs/observability.md)。

## 关键 Crate

| Crate | 作用 |
| --- | --- |
| `eliteforge-agent-framework` | Agent 装配、循环调度、模型和工具执行 |
| `eliteforge-agent-core` | 公共类型、能力接口、CapabilityPack API 和 Plugin 契约 |
| `eliteforge-agent-capability-tools` | 可选文件系统读写、搜索、文本修改，以及独立的通用跨平台系统命令能力包 |
| `eliteforge-agent-official-plugin-skills` | Agent Skills 标准文件系统发现、渐进式加载和可替换 Provider |
| `eliteforge-agent-official-plugin-tools-agents` | 子 Agent 委派与独立 Thread 协作工具 |
| `eliteforge-agent-plugin-kernel` | 服务注册、依赖排序和资源关闭 |
| `eliteforge-agent-official-plugin-pack` | 官方插件组合，一次配置到 Agent |
| `eliteforge-agent-official-plugin-*` | Genai、SQLite、MCP、AG-UI、OTel 等官方实现 |

## 文档

- [快速上手](docs/quickstart.md)：按场景运行 Agent，并查看完整 Rust 代码。
- [架构总览](docs/architecture.md)：分层、装配、执行链和生命周期。
- [插件与扩展开发指南](docs/plugins.md)：引入依赖、实现 Provider、按业务场景选择扩展点。
- [能力包与自动装配](docs/capability-packs.md)：声明扩展点、显式选包、配置工厂、Skills 成员和生命周期。
- [Skills](docs/skills.md)：目录约定、渐进式加载、技能选择和自定义 Provider。
- [扩展点参考](docs/plugin-extension-reference.md)：接口、注册类型、调用时机与多实例规则。
- [模型插件](docs/model-plugins.md)：Genai 配置、协议、模型选择与多模态扩展。
- [DAG 工作流](docs/dag-workflows.md)：节点、并行、分支和恢复。
- [交互与审批](docs/interactions.md)：用户问答、工具审批和跨进程恢复。
- [Agent 协作](docs/collaboration.md)：子 Agent 委派、独立 Threads 和协作边界。
- [可观测](docs/observability.md)：事件、OpenTelemetry 和恢复身份。

## 常用命令

```sh
make format  # 格式化
make lint    # 格式、Clippy、编译和 Rustdoc 检查
make test    # workspace 测试和文档测试
make build   # 构建 release 产物
```
