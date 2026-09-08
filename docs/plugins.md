# Plugin 与扩展点

Plugin 可以注册功能实现、Middleware 和生命周期 Hook。一个 Plugin 可以提供多种服务，
一个 crate 也可以包含多个 Plugin 或供插件使用的组件。

## 插件安装

| 接口 | 作用 |
|---|---|
| `Plugin::id` / `dependencies` | 声明插件名称和依赖；依赖先安装，同等条件按输入顺序 |
| `Plugin::install` | 注册服务和资源清理回调；安装失败时回滚 |
| `PluginContext::provide(T)` | 注册唯一实现，用 `get<T>()` 获取 |
| `PluginContext::contribute(T)` | 注册多个实现，用 `all<T>()` 按顺序获取 |
| `PluginContext::on_shutdown` | 注册异步关闭回调 |
| `PluginContext::on_cleanup` | 注册同步清理回调，供关闭和 Drop 使用 |
| `ResourceScope` | 管理一组资源，按反向顺序释放 |

同一个类型不能混用 provide 和 contribute。`providers<T>()` 可以查询服务来自哪个插件。
源码：[container.rs](../crates/agent-plugin-kernel/src/container.rs)、
[services.rs](../crates/agent-plugin-kernel/src/services.rs)、
[resources.rs](../crates/agent-plugin-kernel/src/resources.rs)。

## Framework 服务扩展点

表中的“单个”使用 provide，“多个”使用 contribute。

### 执行、模型与状态

| 抽象 | 注册类型 | Plugin 入口 | 默认及现有实现 |
|---|---|---|---|
| `AgentLoop` | `LoopService`，单个 | `AgentLoopPlugin` / `ReactPlugin` | 默认 ReactAgentLoop；可选 WorkflowLoop、DagWorkflowLoop |
| `WorkflowNode` | `WorkflowNodeService`，多个 | `WorkflowNodePlugin` | 内置 ReactWorkflowNode；其他节点自行实现 |
| `ModelConnection` / `ModelProvider` | `ModelConnectionService`，多个 | `ModelConnectionPlugin` | 无默认连接；提供 GenaiProvider |
| `ModelSelectionStrategy` | `ModelStrategyService`，单个 | `ModelStrategyPlugin` | 单模型自动使用 FixedModelStrategy；提供 StageModelStrategy、RoutingStrategy |
| `Model` | `ModelService`，单个 | `ModelPlugin` | 兼容入口，转换为 legacy/default 连接 |
| `ConversationStore` | `ConversationStoreService`，单个 | `ConversationStorePlugin` | SqliteStoragePlugin 注册 SqliteConversationStore；无内存回退 |
| `LongTermMemory` | `LongTermMemoryService`，单个 | `LongTermMemoryPlugin` | SqliteStoragePlugin 注册 SqliteLongTermMemory；检索和写入需配置 |
| `InteractionStore` | `InteractionStoreService`，单个 | `InteractionStorePlugin` / `SqliteStoragePlugin` | SqliteStateStore 同时实现 InteractionStore，使用独立交互表 |
| `StateStore` | `StateStoreService`，单个 | `StateStorePlugin` | SqliteStoragePlugin 注册 SqliteStateStore |

连接 ID 和节点 kind 必须唯一。`framework.react` 是保留 kind，自定义节点使用新名称。
一个模型连接可以包含多个模型；一个选择策略可以组合多个子策略。

### 工具、上下文与事件

| 抽象 | 注册类型 | Plugin 入口 | 默认及现有实现 |
|---|---|---|---|
| `ToolProvider` | `ToolProviderService`，多个 | `ToolProviderPlugin` | 默认空 StaticToolProvider；支持静态工具和 MCP |
| `ToolPolicy` | `ToolPolicyService`，多个 | `ToolPolicyPlugin` | 默认空列表；自行实现允许、拒绝或要求审批 |
| `ContextProvider` | `ContextService`，多个 | `ContextPlugin` | 默认空 FixedContext，可配置固定指令 |
| `ContextContributor` | `ContextContributorService`，多个 | `ContextContributorPlugin` | 默认空列表；按模型调用添加上下文 |
| `ContextAssembler` | `ContextAssemblerService`，单个 | `ContextAssemblerPlugin` | 默认 DefaultContextAssembler |
| `ContextBudgetPolicy` | `ContextBudgetPolicyService`，单个 | `ContextBudgetPolicyPlugin` | 默认 IndependentBlockBudget |
| `ContextLayout` | `ContextLayoutService`，单个 | `ContextLayoutPlugin` | 默认指令和历史；显式布局配置每块额度、顺序及 Reducer |
| `ContextReducer` | `ContextReducerService`，单个 | `ContextReducerPlugin` | 设置简单默认布局的历史 Reducer；显式布局使用各块自己的 Reducer |
| `EventSink` | `EventService`，多个 | `EventPlugin` | 默认 NoopEvents；TelemetryPlugin、AgUiPlugin |

ContextProvider 提供基础上下文。ContextContributor 可以根据当前模型、阶段、步骤和预算
生成上下文。ContextReducer 按块处理模型输入，保留原始历史。
MemoryContributor 按当前输入检索长期记忆；MemoryTools 默认提供搜索和读取，写入需显式配置。
ContextLayout 明确排列顺序，各块不借用额度、不按优先级保留；示例见[上下文窗口](context-windows.md)。

工具策略需要执行器支持。ReactAgentLoop、WorkflowLoop 和 DAG 内置 ReAct 节点都支持审批。
自定义节点需要实现相应的策略检查。ContextProviderPlugin 和 EventSinkPlugin 分别是
ContextPlugin、EventPlugin 的别名。

### Middleware 与 Hook

| 抽象 | 注册类型 | Plugin 入口 | 现有实现 |
|---|---|---|---|
| `RunMiddleware` | `RunMiddlewareService` | `RunMiddlewarePlugin` | TelemetryPlugin |
| `WorkflowNodeMiddleware` | `WorkflowNodeMiddlewareService` | `WorkflowNodeMiddlewarePlugin` | TelemetryPlugin |
| `ModelMiddleware` | `ModelMiddlewareService` | `ModelMiddlewarePlugin` | TelemetryPlugin |
| `ToolMiddleware` | `ToolMiddlewareService` | `ToolMiddlewarePlugin` | TelemetryPlugin、ToolOutputOffloader |
| `RunLifecycleHook` | `RunLifecycleHookService` | `RunLifecycleHookPlugin` | 自行实现 on_started / on_finished |

以上都允许多个实现，默认列表为空。Middleware 通过 `intercept + next` 拦截执行；
Hook 只接收生命周期通知，不修改结果。调用顺序和错误处理见 [extensions.md](extensions.md)。

服务定义：[plugins.rs](../crates/agent-framework/src/plugins.rs)、
[models.rs](../crates/agent-framework/src/models.rs)、[state.rs](../crates/agent-framework/src/state.rs)。

## 直接注入的扩展点

以下抽象传给使用它的对象。SQLite 组合插件同时将摘要与附件存储注册为
`SummaryStoreService`、`ArtifactStoreService`，便于其他插件取得共享实例。

| 抽象 | 接入方式 | 现有实现 |
|---|---|---|
| `Tool` | 放入 ToolBinding，由 ToolProvider 返回 | AskUserTool、自定义工具、MCP 工具、ArtifactReadTool |
| `ConversationSession` | 由 ConversationStore.open 返回 | SqliteConversationStore 的会话实现 |
| `TokenEstimator` | ModelConnection.with_token_estimator | 默认 ConservativeTokenEstimator |
| `ModelDiscovery` | 应用调用 discover，筛选后注册模型目录 | GenaiProvider |
| `ModelStream` | 传给模型生成接口 | 运行事件流、增量追踪和静默摘要输出 |
| `SummaryStore` | ModelCompressor.with_store | SqliteSummaryStore |
| `ErrorRedactor` | TelemetryPlugin.with_error_redactor / AgentError.report | 默认 OmitErrorMessages |
| `ArtifactStore` | 注入 ToolOutputOffloader、ArtifactReadTool | SqliteArtifactStore、FileArtifactStore；抽象定义在 agent-context |

RunRequest.extensions 可传递应用数据，例如 TraceParent；这些数据不写入检查点。
模型发现不会自动注册模型，也不会持续刷新目录。

## 已提供的插件和组件

以下实现都需要配置后使用。

| Crate | 实现 | 使用方式 |
|---|---|---|
| `agent-model-genai` | GenaiProvider，使用 genai 0.7.0-beta.21 | 配置协议、地址、认证和模型，通过 provider.plugin(models) 注册 |
| `sqlite-extensions` | ExtensionConfig、SqliteExtensionHost | 独立平台适配包，按连接加载 sqlite-simple / sqlite-vec；不依赖 Framework |
| `agent-storage-sqlite` | SqliteStoragePlugin | 一次配置 SQLite 文件，注册执行、交互、会话、长期记忆、摘要和附件存储，并管理关闭 |
| `agent-tools-human` | AskUserTool、HumanInteractionPlugin | 默认结构化用户问答；支持选项、自由文本和跳过 |
| `agent-state-sqlite` | SqliteStateStore、SqliteExecutionPlugin | 打开数据库，通过 store.plugin() 注册；应用关闭共享 store |
| `agent-tools-mcp` | McpClient，支持 stdio / Streamable HTTP | McpToolsPlugin.shared 使用已有连接；owned 创建并负责关闭连接 |
| `agent-memory` | SqliteConversationStore、SqliteLongTermMemory、MemoryContributor、MemoryTools | 持久化会话、长期记忆和上下文接入 |
| `agent-context` | ModelCompressor、SqliteSummaryStore | 指定摘要模型；按历史块配置压缩，可注入摘要存储 |
| `agent-context` | ToolOutputOffloader | 指定 ArtifactStore 和输出阈值，通过 ToolMiddlewarePlugin 注册 |
| `agent-context` | FileArtifactStore、ArtifactReadTool | 配置存储目录，将读取工具的 binding 加入 ToolProvider |
| `agent-observability` | TelemetryPlugin | 注入 OTel tracer/meter provider；应用配置 exporter、采样和关闭 |
| `agent-agui` | AgUiPlugin | 注册 EventService，使用有界通道转换 AG-UI 事件与 SSE；应用提供 HTTP 接口 |

SQLite 保存工作流检查点，不替代 ConversationStore。附件的访问权限与保留时间由应用管理。
子 Agent 通过 `Agent::run_child` 调用，可封装成工具；目前没有独立的子 Agent 调度插件。

源码：[genai](../crates/agent-model-genai/src/lib.rs)、[SQLite](../crates/agent-state-sqlite/src/lib.rs)、
[MCP](../crates/agent-tools-mcp/src/lib.rs)、[摘要](../crates/agent-context/src/compression.rs)、
[附件](../crates/agent-context/src/artifact.rs)、[OTel](../crates/agent-observability/src/lib.rs)。

## 常见组合

| 需求 | 组合 |
|---|---|
| 对话 Agent | 模型连接 + 默认 ReAct + SqliteStoragePlugin |
| 一个订阅下使用多个模型 | 一个 ModelConnection + 多个 ModelDescriptor + 选择策略 |
| 分阶段选择模型 | StageModelStrategy + WorkflowLoop / DAG |
| 模型失败后切换供应商 | RoutingStrategy + 多个连接，ModelInvoker 负责重试 |
| 并行任务和分支 | DagWorkflowLoop + StateStore + 节点插件 |
| 工具审批和恢复 | ReAct / WorkflowLoop / DAG + SqliteStoragePlugin + ToolPolicy |
| 历史压缩和大输出处理 | ModelCompressor + ToolOutputOffloader + ArtifactReadTool |
| 可观测 | TelemetryPlugin + 应用配置的 OTel provider |

示例：[minimal：使用默认实现创建 Agent](../crates/agent-model-genai/examples/minimal.rs)、
[standalone](../crates/agent-framework/examples/standalone.rs)、
[shared_connection](../crates/agent-model-genai/examples/shared_connection.rs)、
[durable_approval](../crates/agent-state-sqlite/examples/durable_approval.rs)、
[dag](../crates/agent-state-sqlite/examples/dag.rs)。


`EventSink::on_diagnostic` 是三种诊断元数据的同步观测入口（组装成功、组装失败、最终发送），
用于在 span 结束前记录统计，不修改请求或执行结果，也不属于生命周期 Hook。
它必须快速且不进行 I/O；`on_event` 仍在有界异步队列中消费普通事件。

`EventSink::on_published` 按实时发布顺序同步接收完整 EventEnvelope，包含节点执行身份。
它用于快速、非阻塞的输出通道交接，不能进行 I/O 或再次发布事件。`AgUiPlugin` 在这里
消费 `OutputEvent`，独立处理通道容量与协议结束，不经过普通观测队列。
注册和使用见 [AG-UI](agui.md)。

SQLite 核心与原生扩展的分工、目标平台和启用代码见[SQLite 存储](sqlite-storage.md)。
