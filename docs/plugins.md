# Plugin 与扩展点

本文面向使用 Framework 构建 Agent 的应用开发者。阅读本文后，你可以理解 Plugin、Service 和扩展点之间的关系，知道 Framework 未配置时会提供哪些默认行为，能够根据需求选择合适的扩展方式，并找到仓库中各 Plugin 包已经提供的实现。

按场景运行的完整代码见[快速上手](quickstart.md)，执行链和组件边界见[架构说明](architecture.md)。

## 基础概念

### Plugin 是能力的定义

Plugin 通过 ID 唯一标识自己。它在 `AgentBuilder::build()` 阶段安装，可以注册一个服务、提供多个同类实现，或创建需要在 Agent 关闭时释放的资源。

Plugin 不是业务逻辑本身，而是把一项能力接入 Agent 的安装对象。例如，模型 Plugin 把模型调用方式注册给 Framework，SQLite Plugin 把会话和执行状态的存储注册给 Framework。

安装完成后，Agent 只能使用这些 Plugin 注册或提供的能力：

```text
应用 Plugin
    │  注册或添加能力
    ▼
Framework 安装结果
    │
    ▼
Agent 可用的执行循环、模型、存储、工具、模型输入和运行事件
```

Plugin 的职责是把能力安装到 Agent；它不是业务层的路由、权限系统或 HTTP Controller。

### Service 是使用能力的入口

Plugin 安装后，Framework 通过对应的 Service 读取能力。例如：

- `ConversationStore` 保存完整对话记录；
- `StateStore` 保存执行进度并支持恢复；
- `ToolProvider` 在一次运行开始时返回本次可用的工具清单；
- `EventSink` 接收运行事件；
- `ContextAssembler` 组织本次模型请求的上下文。

应用通常不需要直接操作 Service，而是安装提供该 Service 的 Plugin。

`provide` 和 `contribute` 的区别：

| 注册方式 | 含义 | 适合的扩展点 |
|---|---|---|
| `provide(T)` | 整个 Agent 只保留一个实现；重复注册会失败 | 模型选择策略、会话存储、状态存储、上下文组装器 |
| `contribute(T)` | Agent 可以保留多个同类实现，运行时按注册顺序使用 | 应用提供的工具、动态上下文、事件接收器、中间件 |

同一个 Service 类型只能使用其中一种注册方式。每个 Plugin 的 ID 仍必须唯一。

### Framework 负责整体装配

`AgentBuilder::build()` 会先安装应用通过 `.plugin(...)` 提供的 Plugin，再运行 Framework 内部的 `DefaultsPlugin`。`DefaultsPlugin` 只补充尚未提供的基础能力：没有执行循环时加入 `ReactAgentLoop`，没有工具时加入空的 `StaticToolProvider`，没有固定上下文时加入空的 `FixedContext`，没有事件接收器时加入 `NoopEvents`。它不会替换应用已经注册的实现，也不会替应用创建模型连接或业务存储。

表中的“必须”表示 Agent 构建或所选执行方式运行时需要；“视情况”表示只有使用对应场景时需要；“否”表示该能力有 Framework 默认行为，或不影响基础运行。

| 具体能力 | 是否必须 | 注册对象 | Framework 的装配规则 | 说明 |
|---|---|---|---|---|
| Agent 执行循环 | 必须 | `AgentLoop` | `DefaultsPlugin` 在没有其他 `AgentLoop` 时提供 `ReactAgentLoop` | 决定模型调用、工具调用和结束条件的执行顺序 |
| 有向无环工作流节点执行 | 视情况（使用有向无环工作流时） | `WorkflowNode` | `DefaultsPlugin` 提供类型为 `framework.react` 的 `ReactWorkflowNode`，应用可以继续加入其他节点类型 | 在有向无环工作流中执行一个具体节点；节点可以调用模型、工具或业务逻辑 |
| 模型连接与可用模型 | 视情况（执行循环需要调用模型时） | `ModelConnection` | `DefaultsPlugin` 不添加模型连接；Framework 把应用注册的连接和模型描述整理成可选模型清单 | 保存调用供应商所需的适配器、可用模型及模型能力，供 `ModelInvoker` 使用 |
| 模型选择与路由 | 视情况（注册多个可选模型时） | `ModelSelectionStrategy` | `DefaultsPlugin` 不添加选择策略；只有一个模型时 Framework 自动使用 `FixedModelStrategy`，多个模型时使用已注册的策略 | 在多个模型中选择当前阶段要调用的模型，也可以按顺序尝试备用模型 |
| 已有单模型实现兼容 | 否 | `Model` | `DefaultsPlugin` 不添加 `Model`；`ModelPlugin` 会把已有实现包装成 `legacy/default` 连接 | 兼容只有一个 `generate(request)` 方法的旧式模型实现 |
| 对话消息记录 | 必须 | `ConversationStore` | `DefaultsPlugin` 不添加对话消息存储；Agent 构建时需要一个可用实现 | 打开会话、读取历史消息，并追加用户消息、模型回复和工具结果 |
| 执行进度与恢复 | 视情况（执行循环需要保存进度并支持恢复时） | `StateStore` | `DefaultsPlugin` 不添加执行状态存储；需要恢复能力的执行循环使用已注册的实现 | 保存当前阶段、已完成步骤和继续运行所需的数据，并防止同一会话被多个运行同时占用 |
| 用户询问与审批记录 | 视情况（执行循环会等待用户回答或审批时） | `InteractionStore` | `DefaultsPlugin` 不添加交互存储；需要等待用户回答或审批的执行循环使用已注册的实现 | 保存待回答的问题、审批请求、用户答案和处理状态 |
| 跨会话业务记忆 | 否 | `LongTermMemory` | `DefaultsPlugin` 不添加长期记忆；需要跨会话复用事实时使用已注册的实现 | 保存和检索跨会话事实；记忆是否写入由应用或记忆工具决定 |
| 工具清单提供 | 否 | `ToolProvider` | `DefaultsPlugin` 在没有应用提供工具时提供空的 `StaticToolProvider` | 在一次运行开始时告诉 Framework 当前有哪些工具可以交给模型调用 |
| 工具定义与执行 | 否 | `Tool` / `ToolBinding` | `DefaultsPlugin` 不添加具体工具；应用提供的工具由 `ToolProvider` 收集到本次运行的工具清单中 | 定义工具名称、参数格式和执行代码；模型发起调用后由对应代码执行 |
| 工具调用授权 | 否 | `ToolPolicy` | `DefaultsPlugin` 不添加工具授权策略；支持策略检查的执行循环在执行前调用已注册的策略 | 决定一次工具调用是否允许执行，例如审批、白名单和拒绝规则 |
| 固定上下文来源 | 否 | `ContextProvider` | `DefaultsPlugin` 在没有应用提供固定上下文时提供空的 `FixedContext` | 在每次模型调用前提供固定的 system/developer 指令 |
| 动态上下文来源 | 否 | `ContextContributor` | `DefaultsPlugin` 不添加动态上下文来源；组装器使用应用注册的来源生成附加资料 | 根据当前问题生成相关记忆、检索结果或宿主数据 |
| 上下文拼接顺序 | 否 | `ContextLayout` | `DefaultsPlugin` 不添加显式拼接顺序；`DefaultContextAssembler` 使用默认的固定上下文和历史消息顺序 | 规定固定上下文、附加资料和历史消息在模型输入中的顺序及最大长度 |
| 上下文预算分配 | 否 | `ContextBudgetPolicy` | `DefaultsPlugin` 不添加显式预算策略；`DefaultContextAssembler` 默认使用 `IndependentBlockBudget` | 把模型可用输入长度分给不同内容，并检查总长度是否超限 |
| 上下文裁剪与压缩 | 否 | `ContextReducer` | `DefaultsPlugin` 不添加显式裁剪器；`DefaultContextAssembler` 默认裁剪历史并保留其他内容 | 内容过长时保留最近消息、筛选条目或调用摘要模型压缩 |
| 模型输入组装 | 视情况（调用模型时） | `ContextAssembler` | `DefaultsPlugin` 不添加自定义组装器；使用模型连接时 `AgentBuilder` 默认创建 `DefaultContextAssembler` | 把固定上下文、附加资料和会话历史组合成最终发送给模型的消息列表 |
| 运行事件接收 | 否 | `EventSink` | `DefaultsPlugin` 在没有其他事件接收器时提供 `NoopEvents` | 接收运行、模型、工具和上下文事件，用于日志、观测或 UI 推送 |
| 执行拦截与生命周期通知 | 否 | Middleware / `RunLifecycleHook` | `DefaultsPlugin` 不添加 Middleware 或 Hook；Framework 只执行已注册的实现 | Middleware 可以拦截或拒绝执行；Hook 只接收运行开始和结束通知 |

两个需要特别注意的点：

- `ConversationStore` 是构建 Agent 的必需依赖；`StateStore` 和 `InteractionStore` 是否必需，取决于最终选择的 `AgentLoop`。
- `ToolPolicy` 不是自动生效的安全开关。只有支持策略检查的执行循环才会执行它；自定义工作流节点也必须自己调用策略检查。

## 如何安装扩展

### 安装现成 Plugin

```rust
let agent = AgentBuilder::new()
    .plugin(SqliteStoragePlugin::new("./agent.sqlite")) // 传入配置，创建 Plugin
    .plugin(model_provider.plugin(vec![ModelDescriptor::new("my-model")])) // 把模型实现包装成 Plugin
    .plugin(HumanInteractionPlugin) // 不需要配置，类型本身就是 Plugin
    .build()
    .await?;
```

这三种写法最终传给 `.plugin(...)` 的都是 Plugin，区别只在于 Plugin 的配置方式不同：

- `SqliteStoragePlugin::new("./agent.sqlite")`：SQLite Plugin 需要数据库文件路径，所以先调用构造函数创建一个已经配置好的 Plugin。这个 Plugin 会注册会话、执行状态、用户交互、长期记忆等存储。
- `model_provider.plugin(...)`：`model_provider` 是一个已经创建好的模型供应商对象，不是 Plugin。它的 `plugin(models)` 方法把供应商、可用模型列表和连接配置组合成 `ModelConnectionPlugin`。
- `HumanInteractionPlugin`：这是零配置 Plugin，不需要路径、模型列表或其他参数，因此直接使用它的实例名称。

`AgentBuilder::plugin(...)` 接收任何实现了 `Plugin` 的对象，所以这些写法可以在同一条安装链中混用。Plugin 安装时可以注册服务和资源清理动作。如果后续 Plugin 安装失败，已经创建的资源会回滚；Agent 关闭时，Framework 会执行 Plugin 注册的异步关闭动作和同步清理动作。

### 自己实现 Plugin

自定义 Plugin 通常只需要三步：声明 ID、实现 `install`、注册实现。选择 `provide` 还是 `contribute`，取决于这个扩展点是“唯一替换”还是“可叠加组件”。

```rust
struct ApplicationToolProvider;

#[async_trait]
impl Plugin for ApplicationToolProvider {
    fn id(&self) -> &str {
        "app.application-tools"
    }

    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let provider = StaticToolProvider::new(vec![my_tool_binding()]);
        context.contribute(ToolProviderService(Arc::new(provider)))
    }
}
```

依赖关系通过 `Plugin::dependencies()` 声明，依赖的 Plugin 会先安装。如果 Plugin 自己管理连接、线程或临时目录，应在 `install` 中注册 `on_shutdown` 或 `on_cleanup`，让 Framework 在关闭时释放这些资源。

## 扩展点总览

扩展点按 Agent 执行、存储与记忆、工具授权、模型输入构建以及运行事件分组。每个扩展点都列出对应的 `Service` 注册类型和可用实现。

### Agent 执行循环、有向无环工作流节点与模型选择

| 具体能力 | 扩展点 | 注册类型 / 入口 | Framework 或现有实现 |
|---|---|---|---|
| Agent 执行循环 | `AgentLoop` | `LoopService` / `AgentLoopPlugin`、`ReactPlugin` | `ReactAgentLoop`、`WorkflowLoop`、`DagWorkflowLoop` |
| 有向无环工作流节点执行 | `WorkflowNode` | `WorkflowNodeService` / `WorkflowNodePlugin` | `ReactWorkflowNode`、自定义节点 |
| 模型连接与可用模型 | `ModelConnection` | `ModelConnectionService` / `ModelConnectionPlugin` | `agent-model-genai::GenaiProvider` |
| 模型选择与路由 | `ModelSelectionStrategy` | `ModelStrategyService` / `ModelStrategyPlugin` | `FixedModelStrategy`、`StageModelStrategy`、`RoutingStrategy` |
| 已有单模型实现兼容 | `Model` | `ModelService` / `ModelPlugin` | 只为已有 `impl Model` 保留的简化入口 |

### 为什么同时有 `ModelConnection` 和 `Model`

新代码只需要 `ModelConnection`，不需要同时注册 `Model`。即使只有一个模型，也可以把它放入一个只包含一个 `ModelDescriptor` 的连接中。实际安装时使用 `ModelConnectionPlugin`。

```text
推荐路径：ModelConnectionPlugin
    └─ ModelConnection
       ├─ ModelProvider       怎么调用供应商协议
       ├─ ModelDescriptor[]   这个连接有哪些模型、支持什么能力
       └─ TokenEstimator       如何估算输入和输出占用

兼容路径：ModelPlugin
    └─ Model                  只有一个 generate(request) 的旧式实现
             │
             └─ Framework 包装为 ModelConnection("legacy", "default")
```

两者解决的问题不同：

- `ModelConnection` 是模型运行配置。它把一个供应商连接和一个或多个模型列入可选模型清单，供 `ModelSelectionStrategy` 选择；同一个连接可以复用客户端、地址和认证。
- `ModelProvider` 是 `ModelConnection` 内部的协议适配器，负责把 `ModelRequest` 发送给供应商并接收结果。它不是和 `ModelConnection` 并列的另一种接入方式。
- `Model` 是更简单的旧接口，只描述“收到请求后生成结果”。它没有模型 ID、模型能力描述或选择入口，所以 Framework 只能把它固定映射为 `legacy/default`。

因此，“只有 `ModelConnection` 行不行”的答案是：行，而且是新代码的推荐方式。只有在已经有 `impl Model`、希望少改代码时，才使用 `ModelPlugin`。

一个只接入单个模型的连接如下：

```rust
let connection = ModelConnection::new(
    "openai",
    Arc::new(MyModelProvider::new(/* 服务地址、认证信息 */)),
    vec![ModelDescriptor::new("my-model")],
);

let agent = AgentBuilder::new()
    .plugin(ModelConnectionPlugin::new(connection))
    // 还需要按应用场景安装 ConversationStore 等其他必需能力
    .build()
    .await?;
```

`GenaiProvider::plugin(models)` 已经替应用完成了 `ModelProvider` 和 `ModelConnection` 的组装，因此接入 genai 供应商时不需要自己实现这两层。

### 对话消息、执行进度与跨会话记忆

| 具体能力 | 扩展点 | 注册类型 / 入口 | Framework 或现有实现 |
|---|---|---|---|
| 对话消息记录 | `ConversationStore` | `ConversationStoreService` / `ConversationStorePlugin` | `SqliteConversationStore`、`SqliteStoragePlugin` |
| 执行进度与恢复 | `StateStore` | `StateStoreService` / `StateStorePlugin` | `SqliteStateStore`、`SqliteExecutionPlugin`、`SqliteStoragePlugin` |
| 用户询问与审批记录 | `InteractionStore` | `InteractionStoreService` / `InteractionStorePlugin` | `SqliteStateStore`、`SqliteExecutionPlugin`、`SqliteStoragePlugin` |
| 跨会话业务记忆 | `LongTermMemory` | `LongTermMemoryService` / `LongTermMemoryPlugin` | `SqliteLongTermMemory`、`SqliteStoragePlugin` |

四个存储对象分别保存以下内容：

```text
ConversationStore  ── 完整对话记录：用户消息、模型回复、工具调用和工具结果
StateStore         ── 执行状态：当前阶段、已完成步骤和继续运行所需的数据
InteractionStore   ── 待回答的问题、审批请求和用户提交的答案
LongTermMemory     ── 可以跨对话继续使用的应用事实
```

这四种数据分别服务于不同阶段：对话存储保留完整消息，执行状态用于恢复运行，交互存储保存等待用户处理的事项，长期记忆保存可以跨会话复用的事实。模型本次要读取哪些内容，由模型输入构建流程决定；这个过程不会替换对话存储中的完整记录。

`agent-memory` 不会自动从对话中抽取事实。写入长期记忆需要应用显式调用 `LongTermMemory` 或显式启用带写入能力的 `MemoryTools`。

### 工具清单与调用授权

| 具体能力 | 扩展点 | 注册类型 / 入口 | Framework 或现有实现 |
|---|---|---|---|
| 工具清单提供 | `ToolProvider` | `ToolProviderService` / `ToolProviderPlugin` | `StaticToolProvider`、`McpClient` |
| 工具定义与执行 | `Tool` | 放入 `ToolBinding`，由 `ToolProvider` 返回 | `AskUserTool`、`MemoryTools` 生成的绑定、MCP 工具、`ArtifactReadTool`、自定义工具 |
| 工具调用授权 | `ToolPolicy` | `ToolPolicyService` / `ToolPolicyPlugin` | Framework 提供接口；应用实现审批、白名单和拒绝规则 |

`ToolProvider` 在一次运行开始时向 Framework 返回本次可用的工具清单。代码接口的方法名是 `lease()`，这里表示“获取本次清单”，不是租借外部资源。`ToolPolicy` 决定工具执行前是否允许调用，二者不是一回事。`HumanInteractionPlugin` 只提供 `AskUserTool`，不会自动要求所有工具都经过审批。

### 模型输入构建

这里的“输入”指本次要发送给模型的消息，不是数据库里的完整会话记录。下面所说的“信息块”，就是一段可以单独设置顺序、最大长度和裁剪方式的输入内容，例如固定上下文、相关记忆或历史消息。Framework 每次调用模型时，按以下路径生成这份输入：

```text
固定上下文 ─────────────┐
当前问题触发的附加资料 ──┼─► 按顺序分块、分配输入长度、必要时裁剪
完整会话中的历史消息 ────┘                 │
                                           ▼
                              本次模型请求的消息列表
```

例如，一个带长期记忆的 Agent 会这样工作：

```text
固定上下文“用中文回答”        → 固定上下文
用户问题“继续上次的方案”       → MemoryContributor 查询相关记忆
会话历史                       → ConversationStore 读取历史消息
固定上下文 + 记忆信息 + 历史消息 → ContextAssembler 生成模型输入
```

| 模型输入中的内容 | 扩展点 | 它实际做什么 | 注册入口与实现 |
|---|---|---|---|
| 固定上下文和身份 | `ContextProvider` | 每次模型调用前提供固定的 system/developer 消息，例如角色、回答格式和安全规则 | `ContextPlugin` + `FixedContext`；也可以实现自定义 `ContextProvider` |
| 根据当前问题生成的附加上下文 | `ContextContributor` | 每次组装时读取当前问题和会话历史，查询相关资料并返回一段带名称的附加内容，例如长期记忆或检索结果 | `ContextContributorPlugin` + `MemoryContributor` / `FixedContextContributor` |
| 上下文拼接顺序和大小 | `ContextLayout` | 指定固定上下文、附加资料、历史消息在模型输入中的顺序，并给每类内容设置最大长度 | `ContextLayoutPlugin` + `ContextBlockConfig` |
| 总输入长度如何分给各块 | `ContextBudgetPolicy` | 为每类内容分配输入长度，并检查总长度不超过当前模型的输入上限 | `ContextBudgetPolicyPlugin` + `IndependentBlockBudget` |
| 某块超出长度时怎么处理 | `ContextReducer` | 按策略保留最近消息、筛选部分条目，或用摘要替换较长的历史内容 | `ContextReducerPlugin` + `RecentConversation` / `SelectContextEntries` / `ModelCompressor` |
| 把所有内容组成本次请求 | `ContextAssembler` | 读取固定上下文、附加资料和会话历史，按拼接顺序执行长度限制和裁剪，生成最终消息列表 | `ContextAssemblerPlugin` + `DefaultContextAssembler`；也可以自定义 |

没有动态上下文时，`DefaultContextAssembler` 只处理两部分：固定上下文和对话历史。接入 `ContextContributor` 后，需要在 `ContextLayout` 中同时列出固定上下文、每个附加上下文和历史消息，并为它们设置最大长度。

如果使用自定义 `ContextAssembler`，固定上下文、附加资料、拼接顺序、长度限制和裁剪策略都由这个组装器自己管理，不能再通过其他 Context Plugin 额外配置。

### 运行事件、执行拦截与生命周期通知

| 具体能力 | 扩展点 | 注册类型 / 入口 | Framework 或现有实现 |
|---|---|---|---|
| 运行事件接收 | `EventSink` | `EventService` / `EventPlugin` | `TelemetryPlugin`、`AgUiPlugin`、自定义 `EventSink` |
| Agent 运行拦截 | `RunMiddleware` | `RunMiddlewareService` / `RunMiddlewarePlugin` | `TelemetryPlugin`、自定义鉴权或限流 |
| 模型调用拦截 | `ModelMiddleware` | `ModelMiddlewareService` / `ModelMiddlewarePlugin` | `TelemetryPlugin`、自定义观测 |
| 工具调用拦截 | `ToolMiddleware` | `ToolMiddlewareService` / `ToolMiddlewarePlugin` | `TelemetryPlugin`、`ToolOutputOffloader` |
| 有向无环工作流节点拦截 | `WorkflowNodeMiddleware` | `WorkflowNodeMiddlewareService` / `WorkflowNodeMiddlewarePlugin` | `TelemetryPlugin`、自定义观测 |
| Agent 开始/结束通知 | `RunLifecycleHook` | `RunLifecycleHookService` / `RunLifecycleHookPlugin` | 应用自定义实现 |

Middleware 和 Hook 的边界如下：

```text
Middleware:  intercept(input, next) -> result
             可以拒绝、包裹执行、观察结果

Hook:        on_started(context)
             on_finished(context, result)
             只接收开始和结束通知，不替换执行结果
```

需要鉴权、限流、修改请求或阻止执行时使用 Middleware 或 `ToolPolicy`。只需要记录“开始了”和“结束了”时使用 Hook。Hook 的异常会被隔离并转成事件，不会把运行结果改成另一个结果。

## 无需单独安装 Plugin 的直接组件

上一节列的是可以直接传给 `AgentBuilder::plugin(...)` 的安装对象。本节列的是各 crate 提供的可复用组件。这里的 crate 指仓库中的 Rust 子包，例如 `agent-memory` 或 `agent-context`。这些组件通常需要作为参数交给 Plugin，或由应用组合进自己的 Plugin；它们本身不会单独参与 Agent 装配。

这些类型本身不能直接作为 Plugin 安装。实现了 Framework trait 的类型，需要交给 `agent-framework` 提供的 Plugin 注册；其他类型则通过构造函数、模型调用参数或本次运行扩展数据传入：

| 来源 crate | 具体组件 | 接入方式 | 用途 |
|---|---|---|---|
| `agent-memory` | `SqliteConversationStore`、`SqliteLongTermMemory` | 分别交给 `ConversationStorePlugin`、`LongTermMemoryPlugin`；也可以直接使用 `SqliteStoragePlugin` | SQLite 对话消息和跨会话记忆 |
| `agent-memory` | `MemoryContributor` | 交给 `ContextContributorPlugin`，并在 `ContextLayout` 中配置它在模型输入中的位置和最大长度 | 按当前问题检索长期记忆，生成附加上下文 |
| `agent-memory` | `MemoryTools` | 用 `bindings()` 生成 `ToolBinding`，再交给 `ToolProviderPlugin` 或 `StaticToolProvider` | 向模型提供记忆搜索、读取；写入和删除需要显式开启 |
| `agent-framework` | `TokenEstimator` | 通过 `ModelConnection::with_token_estimator` 设置 | 估算模型请求、输入内容和输出内容的长度 |
| `agent-model-genai` | `GenaiProvider` 的 `ModelDiscovery` | 应用筛选查询结果后调用 `GenaiProvider::plugin(models)` 注册 | 查询供应商当前支持的模型列表 |
| `agent-framework` | `ModelStream` | Framework 在模型调用时传入 | 接收模型实时返回的文本、推理过程和其他片段 |
| `agent-context` | `SqliteSummaryStore`、`SummaryStore` | 传给 `ModelCompressor::with_store` | 持久化模型生成的历史摘要 |
| `agent-context` | `SqliteArtifactStore`、`FileArtifactStore`、`ArtifactStore` | 注入 `ToolOutputOffloader` 或 `ArtifactReadTool` | 保存过大的工具输出，并按范围读取原始内容 |
| `agent-context` | `ArtifactReadTool` | 用 `ArtifactStore` 构造后加入 `ToolProvider` | 以受限范围读取附件 |
| `agent-framework` | `ErrorRedactor` | 传给 `TelemetryPlugin::with_error_redactor` | 控制观测数据中哪些错误文本可以外发；Framework 提供 `OmitErrorMessages` |
| `agent-framework` | `RunRequest.extensions` | 由应用写入和读取 | 携带本次运行的额外数据，例如链路跟踪信息；不会自动保存到执行状态 |

## 可直接安装的 Plugin 包

这里需要区分两个概念：

- **crate** 是 Rust 的发布单元，例如 `agent-storage-sqlite`；一个 crate 可以提供多个类型。
- **Plugin** 是可以传给 `AgentBuilder::plugin(...)` 的安装对象，负责把实现注册到 Agent，并管理自己的资源生命周期。

仓库提供的实现确实分布在多个 Plugin 包中，但这些包属于可选能力，不会自动全部安装。`DefaultsPlugin` 只负责 Agent 的基础兜底；模型供应商、数据库、外部工具、用户交互、观测和 UI 都由相应的 Plugin 包接入。

| 来源 crate | 可安装对象 | 安装后提供的能力 | 适用场景 |
|---|---|---|---|
| `agent-framework` | `ReactPlugin`、`AgentLoopPlugin` | Agent 执行循环 | 使用内置 ReAct，或注册自定义 `AgentLoop` |
| `agent-framework` | `ModelConnectionPlugin`、`ModelStrategyPlugin`、`WorkflowNodePlugin` | 模型连接、模型选择策略、有向无环工作流节点 | 应用实现自定义模型适配、路由策略或工作流节点 |
| `agent-framework` | `ConversationStorePlugin`、`StateStorePlugin`、`InteractionStorePlugin`、`LongTermMemoryPlugin` | 对话记录、执行状态、用户交互、长期记忆 | 应用已有其他数据库或自定义存储实现 |
| `agent-framework` | `ToolProviderPlugin`、`ContextPlugin`、`ContextContributorPlugin`、`ContextLayoutPlugin`、`ContextBudgetPolicyPlugin`、`ContextReducerPlugin`、`ContextAssemblerPlugin`、`ToolPolicyPlugin` | 工具清单、固定上下文、动态上下文、模型输入组装、工具授权 | 应用自行提供工具、上下文来源或安全策略 |
| `agent-framework` | `EventPlugin`、`RunMiddlewarePlugin`、`ModelMiddlewarePlugin`、`ToolMiddlewarePlugin`、`WorkflowNodeMiddlewarePlugin`、`RunLifecycleHookPlugin` | 运行事件、执行拦截、开始/结束通知 | 接入日志、鉴权、限流、观测或业务回调 |
| `agent-model-genai` | `GenaiProvider::plugin(models)` 返回 `ModelConnectionPlugin` | 一个供应商连接和它的可用模型清单 | 使用 genai 适配的模型供应商；应用提供服务地址和认证信息 |
| `agent-storage-sqlite` | `SqliteStoragePlugin` | 同一个 SQLite 文件中的执行状态、交互、对话、长期记忆、摘要和附件存储 | 希望一次安装一套完整的 SQLite 持久化能力 |
| `agent-state-sqlite` | `SqliteStateStore::plugin()` 返回 `SqliteExecutionPlugin` | 同一个 SQLite 数据库同时提供 `StateStore` 和 `InteractionStore` | 只需要保存执行进度和用户交互记录 |
| `agent-tools-human` | `HumanInteractionPlugin` | 提供 `AskUserTool` 的工具清单 | 让模型向用户发起结构化问题；宿主负责展示问题并提交答案 |
| `agent-tools-mcp` | `McpToolsPlugin::shared(...)` / `owned(...)` | 把 MCP 服务提供的工具加入本次 Agent 运行 | `shared` 复用应用已经建立的连接；`owned` 由 Plugin 创建并负责关闭连接 |
| `agent-observability` | `TelemetryPlugin` | 记录运行、模型、节点、工具拦截和事件 | 宿主提供 OpenTelemetry 的链路和指标后端 |
| `agent-agui` | `AgUiPlugin` | 将运行事件转换为 AG-UI 的实时事件流 | 宿主提供 HTTP 路由、授权和断线后的历史重放 |

`sqlite-extensions` 不是 Plugin 包，而是 SQLite 原生扩展加载组件；它由 SQLite 存储实现按需使用。`SqliteStoragePlugin` 是便捷组合，不代表 Framework 依赖 SQLite；应用也可以只安装自定义的 `ConversationStorePlugin`、`StateStorePlugin` 等。

## 常见组合

| 目标 | 建议组合 |
|---|---|
| 最小可运行对话 Agent | `SqliteStoragePlugin` + `GenaiProvider::plugin(...)`；使用 `ReactAgentLoop` |
| 注册应用内工具 | `ToolProviderPlugin` + `StaticToolProvider` |
| 接入 MCP 工具服务 | `McpToolsPlugin::shared` 或 `McpToolsPlugin::owned` |
| 让模型向用户提问 | `HumanInteractionPlugin` + 支持交互存储的执行循环 |
| 工具审批与恢复 | `ReactAgentLoop` / `WorkflowLoop` / `DagWorkflowLoop` + `ToolPolicyPlugin` + `StateStorePlugin` / `InteractionStorePlugin` |
| 使用长期记忆 | `LongTermMemoryPlugin` + `ContextContributorPlugin::new(...)` + `ContextLayoutPlugin` |
| 历史压缩 | `ContextReducerPlugin::new(ModelCompressor)`，可选 `SqliteSummaryStore` 缓存摘要 |
| 大工具输出 | `ToolMiddlewarePlugin::new(...)` + `ArtifactStore` + `ToolProviderPlugin::new(...)` |
| UI / 实时事件流 | `AgUiPlugin`；宿主提供 HTTP 层 |
| 观测与指标 | `TelemetryPlugin` + 宿主配置的 OpenTelemetry 链路和指标后端 |
| 自定义执行流程 | `AgentLoopPlugin`；使用有向无环工作流时再加 `WorkflowNodePlugin` |

可直接运行的示例：

- [最小模型连接示例](../crates/agent-model-genai/examples/minimal.rs)
- [独立 Framework 示例](../crates/agent-framework/examples/standalone.rs)
- [共享模型连接](../crates/agent-model-genai/examples/shared_connection.rs)
- [可恢复审批](../crates/agent-state-sqlite/examples/durable_approval.rs)
- [有向无环工作流](../crates/agent-state-sqlite/examples/dag.rs)

## 继续阅读

- [扩展点的执行顺序与约束](extensions.md)
- [上下文窗口配置](context-windows.md)
- [SQLite 存储](sqlite-storage.md)
- [AG-UI 集成](agui.md)
- [架构说明](architecture.md)
