# Framework 架构

EliteForge Agent Framework 是可组装、可恢复的 Agent 运行时。它管理执行、状态、工具、协作和事件；
上层应用管理业务身份、权限、凭据、订阅和 UI。

本文使用中文解释概念。反引号内的英文是源码中的类型、方法、字段或枚举值。

## 1. 系统边界

框架只实现通用运行语义。模型协议、存储、MCP、技能、AG-UI 和遥测由插件提供。

```mermaid
flowchart LR
    App[上层应用<br/>身份、权限、凭据、UI] --> Builder[AgentBuilder]

    subgraph Runtime[框架运行时]
        Builder --> Agent[Agent<br/>运行与关闭]
        Agent --> Loop[AgentLoop<br/>ReAct / 工作流 / DAG]
        Loop --> Model[ModelInvoker]
        Loop --> Tool[ToolInvoker]
        Agent --> Collaboration[AgentCollaborationRuntime<br/>子 Agent / Thread]
    end

    subgraph Plugins[插件]
        Models[模型连接]
        Tools[工具 / MCP / Skills]
        Stores[会话 / 状态 / 交互存储]
        Outputs[AG-UI / Telemetry]
    end

    Model --> Models
    Tool --> Tools
    Loop --> Stores
    Agent --> Outputs
    Collaboration --> Agent

    Runtime -. 依赖契约 .-> Core[eliteforge-agent-core<br/>公共契约]
    Plugins -. 实现契约 .-> Core
    Kernel[插件容器<br/>依赖、注册、关闭] --> Builder
```

| 模块 | 职责 |
|---|---|
| `eliteforge-agent-core` | 定义模型、工具、状态、交互、协作和事件契约 |
| `eliteforge-agent-plugin-kernel` | 排序插件依赖，注册服务，回滚安装，关闭资源 |
| `eliteforge-agent-framework` | 组装 Agent，执行运行循环，管理预算、恢复、协作和关闭 |
| 配套插件 | 实现模型、存储、工具、AG-UI 和遥测 |
| 上层应用 | 实现业务权限、配额、路由、凭据和交互界面 |

## 2. 服务组装

`AgentBuilder` 接受单个插件、`PluginPack` 和 `CapabilityPack`。`build().await` 统一处理依赖、
安装、就绪回调和运行时构造。

```mermaid
flowchart LR
    Input[单个插件 / 插件包 / 能力包] --> Resolve[解析依赖图]
    Resolve --> Validate[校验 ID、依赖、环和数量语义]
    Validate --> Install[按稳定拓扑顺序安装]
    Install --> Ready[按安装顺序执行就绪回调]
    Ready --> Services[读取运行时服务]
    Services --> Agent[Agent]
    Install -. 失败 .-> Rollback[逆序回滚已安装资源]
```

`CapabilityPack` 只创建已选能力。未选能力不运行构造函数，也不创建业务对象。

| 数量语义 | 注册 | 读取 | 示例 |
|---|---|---|---|
| 唯一服务 | `provide(T)` | `get<T>()` | `StateStore`、`ModelSelectionStrategy` |
| 多个贡献 | `contribute(T)` | `all<T>()` | `ToolProvider`、中间件、`EventSink` |

同一类型不能同时使用两种数量语义。详见 [插件与扩展点](plugins.md) 和
[能力包与自动装配](capability-packs.md)。

## 3. 执行模型

一次执行由四层组成：`Agent` 管理运行外壳，`AgentLoop` 推进状态机，调用器约束外部调用，
存储服务保存可恢复状态。

### 3.1 运行身份

| 身份 | 语义 |
|---|---|
| `session_id` | 上层应用定义的会话。同一 Agent 实例内同时只有一个活动运行 |
| `run_id` | 一次逻辑运行。恢复时保持不变 |
| `execution_id` | 一次实际执行尝试。每次 `start()` 都生成新值 |
| `ToolCall.invocation_id` | 框架分配的工具调用身份，跨审批、执行和恢复保持稳定 |
| `RunLineage` | 记录子 Agent 树的根运行、父运行和嵌套深度。独立 Thread 不进入该树 |

### 3.2 运行生命周期

`Agent::start` 先完成同步准入，再启动异步执行。

```mermaid
sequenceDiagram
    participant App as 上层应用
    participant Agent
    participant Registry as 运行登记表
    participant Runner as RunNext / AgentLoop
    participant Events as 事件管线

    App->>Agent: start(request)
    Agent->>Agent: 校验请求和 Tokio 运行时<br/>并准备 RunOutputFactory
    Agent->>Registry: 检查 Agent 是否已关闭<br/>以及会话是否已在运行
    Agent->>Agent: 创建 run_id、execution_id<br/>截止时间和用量预算
    Agent->>Events: 启动 ObserverDispatcher
    Agent->>Events: 同步发布 RunStarted
    Agent-->>App: RunHandle

    Agent->>Runner: 开始钩子 → 运行中间件 → AgentLoop
    Runner-->>Agent: RunResult 或 AgentError
    Agent->>Agent: 取消子运行，关闭 Run ResourceScope
    Agent->>Agent: on_finished
    Agent->>Events: RunResult 输出与终态事件
    Agent->>Events: 限时排空观测队列
    Agent->>Registry: 回收会话登记
    Agent-->>App: 权威结果
```

运行具有以下保证：

- `start()` 返回前已发布 `RunStarted`。
- `RunNext` 同时监听取消信号和截止时间。截止时间是本次执行允许运行到的最晚时刻。
- 执行停止后，`RunNext` 最多再等待 `cleanup_timeout` 指定的时间，让中间件和运行循环完成协作清理。
- 丢弃 `RunHandle` 会取消运行；丢弃 `RunControl` 不会。
- `RunLifecycleHook` 只观察开始和结束；`RunMiddleware` 才能包裹执行。

### 3.3 执行器

| 执行器 | 控制流 | 持久化依赖 |
|---|---|---|
| `ReactAgentLoop` | 单阶段模型—工具循环 | `StateStore`、`ConversationStore`、`InteractionStore` |
| `WorkflowLoop` | 多个串行 ReAct 阶段 | `StateStore`、`InteractionStore` |
| `DagWorkflowLoop` | 节点、并行、分支、汇合和节点恢复 | `StateStore`；内置 ReAct 节点还需要 `InteractionStore` |
| 自定义 `AgentLoop` | 由实现定义 | 由实现声明 |

### 3.4 ReAct 与工作流

`ReactAgentLoop` 和 `WorkflowLoop` 共用 `ModelExecutionLoop`。前者读写完整会话；后者把消息记录
保存在执行检查点中。两者使用同一恢复协议。

执行检查点记录当前进度、预算和待处理交互。状态存储会为同一个 `run_id` 发放一把带有效期的运行锁，
源码类型为 `ExecutionLease`。它不锁线程；只有获得锁的执行器才能修改该运行的状态。
执行器失联后，锁会自动过期，其他执行器才能接管。每次接管都会生成更大的锁版本号，
因此旧执行器即使恢复，也无法覆盖新状态。

```mermaid
flowchart TD
    Start[运行或恢复] --> Load{首次运行?}
    Load -->|是| Create[创建执行检查点<br/>保存请求与用户消息]
    Load -->|否| Existing[读取原 run_id 的执行检查点]
    Create --> Terminal
    Existing --> Terminal{执行检查点已终态?}
    Terminal -->|是| Replay[幂等返回已保存结果]
    Terminal -->|否| Lease[获取运行锁<br/>再次读取检查点]
    Lease --> Restore[恢复预算、限制、工具可见性和撤销状态]
    Restore --> Apply[应用交互答案]
    Apply --> Guard{当前执行位置}
    Guard -->|存在 Started 工具| Failed[执行失败]
    Guard -->|存在待处理交互| Await[AwaitingInput]
    Guard -->|Model| Invoke[执行模型步骤]
    Invoke -->|最终内容| Next{还有阶段或 steering?}
    Next -->|是| Guard
    Next -->|否| Done[Completed]
    Invoke -->|tool calls| Plan[分配 invocation_id<br/>持久化整批调用]
    Plan --> Tools[执行工具批次]
    Tools --> Guard
```

恢复不使用旧内存。执行器先获取运行锁，再重新读取执行检查点。存储只接受当前锁版本的写入，
因此旧执行无法覆盖新状态。
持久化的预算和限制是权威上限；恢复请求只能收紧它们。

### 3.5 模型调用

`ModelInvoker` 每次尝试都重新选择模型并组装上下文。

```mermaid
sequenceDiagram
    participant Runner as AgentLoop
    participant Select as ModelSelectionStrategy
    participant Context as ContextAssembler
    participant Model as ModelMiddleware / ChatModel
    participant Budget as UsageLedger
    participant Events as 事件与输出

    Runner->>Select: 阶段、步骤、尝试次数、工具、历史消息
    Select-->>Runner: ModelRef
    Runner->>Context: 指令、消息快照、模型容量
    Context-->>Runner: ContextWindow + 组装报告
    Runner->>Events: ContextAssembled、ModelSelected、ModelStarted
    Runner->>Events: StepStarted、MessageStarted
    Runner->>Model: ChatRequest
    Model->>Budget: 预留模型用量并持久化
    Model->>Events: ModelDispatched
    Model-->>Events: 文本与推理增量
    Model-->>Runner: ChatResponse 或 ModelError
    alt 成功
        Runner->>Budget: 结算用量
        Runner->>Events: MessageFinished、ModelUsage
    else 失败
        Runner->>Events: MessageAborted
    end
    Runner->>Events: StepFinished
```

| 结果 | 处理 |
|---|---|
| 成功 | 校验输出，为工具调用分配框架级 `invocation_id` |
| 短暂错误 | 仅在未输出任何增量且未达重试上限时重试 |
| 上下文溢出 | 仅在未输出增量时重新组装；组装结果未变则失败 |
| 其他错误 | 立即返回，不重放已可见输出 |

`ContextAssembler` 按 `ContextLayout` 组织指令、历史消息和贡献块，再由 `ContextBudgetPolicy`
分配预算、`ContextReducer` 裁剪或摘要。原始会话不会因组装而被改写。

### 3.6 工具执行与大结果转存

工具批次和 `ArtifactStore` 解决不同问题。

| 机制 | 作用 |
|---|---|
| 工具批次 | 先保存执行意图，再调用可能产生副作用的工具 |
| `ArtifactStore` | 把过大的工具结果移出模型上下文，同时保留完整原文 |

```mermaid
flowchart TD
    Calls[模型返回工具批次] --> Plan[持久化模型回复草案<br/>与计划执行项]
    Plan --> Policy[逐项校验工具绑定和参数结构<br/>执行整批 ToolPolicy]
    Policy --> Decision{当前调用决策}
    Decision -->|拒绝| Rejected[记录该调用的错误结果]
    Decision -->|需要审批| Waiting[标记待审批<br/>创建交互请求]
    Decision -->|允许 / 已批准| Ready[标记可派发]
    Rejected --> Persist[持久化整批策略结果]
    Waiting --> Persist
    Ready --> Persist
    Persist --> Pending{存在交互请求?}
    Pending -->|是| Awaiting[AwaitingInput<br/>整批不派发]
    Pending -->|否| Candidates{存在待执行调用?}
    Candidates -->|否| Commit
    Candidates -->|是| Dispatch{所有待执行调用均允许并行?}
    Dispatch -->|是| Parallel[先持久化全部开始状态<br/>再受限并发]
    Dispatch -->|否| Serial[每项先持久化开始状态<br/>再串行执行]
    Parallel --> Invoke[ToolInvoker<br/>撤销、参数、并发控制、中间件]
    Serial --> Invoke
    Invoke --> Outcome{执行结果}
    Outcome -->|结果确定| Finished[持久化完成状态]
    Outcome -->|需要用户输入| Input[持久化交互请求]
    Outcome -->|结果无法确定| Failed[执行失败]
    Finished --> Commit[原子追加模型和工具消息]
    Input --> Commit
    Commit --> Output[发布 ToolResult]
    Output --> Followup{存在交互请求?}
    Followup -->|是| Awaiting
    Followup -->|否| Model[返回模型执行位置]
```

`Deny` 优先于 `RequireApproval`。任一调用等待审批时，整批工具都不派发。
工具派发后发生取消、超时或结果丢失时，运行以取消或错误结束，不自动重放工具。

#### 大结果如何转存

仅注册 `ArtifactStore` 不会改变工具结果。还必须安装 `ToolOutputOffloader` 数据转储中间件，并配置转存阈值。
工具执行完成后，中间件按以下流程处理 `ToolOutput`：

例如，工具返回 10 MB JSON 时，`ArtifactStore` 保存完整的 10 MB；会话只保存预览和
`artifact:<id>`。模型需要更多内容时，再按范围读取。

```mermaid
flowchart TD
    Tool[工具返回 ToolOutput<br/>content、structured、is_error、effect] --> Offloader[ToolOutputOffloader]
    Offloader --> Size{content 与 structured<br/>的总大小超过阈值?}
    Size -->|否| Inline[保留原始 ToolOutput]
    Size -->|是| Serialize[把完整 ToolOutput<br/>序列化为 JSON]
    Serialize --> Store[ArtifactStore.put<br/>保存完整结果]
    Store --> Reference[生成 artifact:id 引用]
    Reference --> Preview[生成精简 ToolOutput<br/>content = 预览与引用<br/>structured = 空<br/>保留 is_error 与 effect]
    Inline --> Persist[写入执行检查点和会话]
    Preview --> Persist
    Persist --> Model[下一次模型调用只读取内联结果]
    Store -->|保存失败| Failed[执行失败]
```

转存完成后，各位置保存的内容不同：

| 位置 | 保存内容 |
|---|---|
| `ArtifactStore` | 完整 `ToolOutput` JSON |
| 执行检查点和会话 | 预览、`artifact:<id>` 引用和 `is_error`；`structured` 为空 |
| 模型可见内容 | 预览与引用；不会自动读取完整结果 |

`result_reference` 保存 `artifact:<id>`、数据类型和字节数。它只是定位信息，不是访问凭据；
上层应用必须为每个 Agent 提供权限范围正确的 `ArtifactStore`。

#### 如何读取完整结果

需要读取原文时，应用必须注册 `ArtifactReadTool`。模型使用引用中的附件 ID 和读取范围调用该工具；
工具从同一个 `ArtifactStore` 分页读取数据。框架不会自动把完整结果重新放回模型上下文。

```mermaid
sequenceDiagram
    participant Model as 模型
    participant Invoker as ToolInvoker
    participant Reader as ArtifactReadTool
    participant Store as ArtifactStore

    Model->>Invoker: 调用读取工具<br/>id、offset、length
    Invoker->>Reader: 校验参数并执行
    Reader->>Store: read(id, offset, length)
    Store-->>Reader: 数据、next_offset、eof
    Reader-->>Invoker: UTF-8 文本或 Base64 数据
    Invoker-->>Model: 作为新的工具结果进入会话
```

如果 `ArtifactStore.put` 失败，运行以错误结束，已执行的工具不会自动重放。

### 3.7 DAG 调度

```mermaid
flowchart TD
    Start[编译并校验 DAG] --> Lease[创建或读取执行检查点<br/>获取运行锁后再读取]
    Lease --> Scan[按稳定拓扑顺序扫描]
    Scan --> Ready{节点是否就绪?}
    Ready -->|否| Wait[等待前驱]
    Ready -->|路径未选中| Skip[持久化 Skipped]
    Ready -->|是| Intent[先持久化 Running<br/>与节点执行身份]
    Intent --> Node[WorkflowNodeMiddleware<br/>→ WorkflowNode.execute]
    Node -->|Completed| Success[持久化 Succeeded]
    Node -->|AwaitingInput / Limit| Suspend[持久化 Suspended]
    Node -->|错误| Abort[中止并回收兄弟任务]
    Success --> Scan
    Suspend --> Result[汇总最高优先级暂停状态]
    Skip --> Scan
    Wait --> Result
    Result -->|全部成功/跳过| Done[Completed]
    Abort --> Failed[执行失败或取消]
```

DAG 并发受 `max_parallel_nodes` 限制。内置 ReAct 节点把状态嵌入父 DAG 执行检查点，不单独获取运行锁。

### 3.8 终止状态

`RunFinished` 表示执行器返回了 `RunResult`，不等于任务成功。

| `RunStatus` | 语义 | AG-UI |
|---|---|---|
| `Completed` | 任务完成 | `RUN_FINISHED(success)` |
| `AwaitingInput` | 等待交互答案 | `RUN_FINISHED(interrupt)` |
| `StepLimit` | 模型步数达限，任务未完成 | `RUN_ERROR(AGENT_LIMIT_REACHED)` |
| `ToolCallLimit` | 工具调用数达限，任务未完成 | `RUN_ERROR(AGENT_LIMIT_REACHED)` |

`AgentError::Cancelled` 产生 `RunCancelled`；其他错误产生 `RunFailed`。`ThreadStatus` 保留上表所有状态，
不会把限额或等待输入状态压成 `Completed`。

## 4. 持久化与恢复

会话、运行状态和交互分别存储。实时事件不参与恢复。

```mermaid
flowchart LR
    Conversation[ConversationStore<br/>完整消息] --> Context[ContextAssembler<br/>本次模型输入]
    State[StateStore<br/>检查点、执行位置、节点、预算] --> Resume[恢复执行]
    Interaction[InteractionStore<br/>请求、答案结构、答案] --> Resume
    Event[EventSink<br/>实时通知] -. 不是恢复依据 .-> UI[界面 / SSE / 遥测]
```

| 存储 | 内容 |
|---|---|
| `ConversationStore` | 完整会话消息。上下文裁剪不改写原始会话 |
| `StateStore` | 执行检查点、执行位置、节点/工具状态、预算和持久化事件日志 |
| `InteractionStore` | 问题、审批、`response_schema`、工具调用和答案 |
| `SummaryStore` | 由历史生成的上下文摘要；不替代原始会话 |
| `ArtifactStore` | 大结果和附件引用 |

运行锁解决一个问题：同一 `run_id` 不能被两个执行器同时修改。

```mermaid
sequenceDiagram
    participant A as 执行者 A
    participant Store as 状态存储
    participant B as 执行者 B

    A->>Store: 请求 run_id 的运行锁
    Store-->>A: 获得锁版本 1
    B->>Store: 请求同一运行锁
    Store-->>B: 锁已被占用
    Note over A: 进程失效，锁自动过期
    B->>Store: 重新请求运行锁
    Store-->>B: 获得锁版本 2
    B->>Store: 使用锁版本 2 写入
    Store-->>B: 接受
    A->>Store: 使用旧锁版本 1 写入
    Store-->>A: 拒绝
```

存储写入被取消时，框架不猜测事务是否已提交。再次执行时重新读取检查点；
若发现已开始但未保存结果的工具，运行以错误结束。

## 5. 用户交互

交互记录用于工具审批和运行中问答。回答先持久化，恢复再启动。

```mermaid
sequenceDiagram
    participant Runner as AgentLoop
    participant State as StateStore
    participant Interaction as InteractionStore
    participant App as 上层应用
    participant Agent

    Runner->>State: 保存 AwaitingInput 与执行位置
    Runner->>Interaction: 幂等保存 InteractionRequest
    Runner-->>App: RunResult(AwaitingInput)
    App->>Agent: answer(session_id, interaction_id, value)
    Agent->>Interaction: 校验答案结构并原子保存
    Interaction-->>Agent: RunRequest { resume: ResumeRequest(run_id) }
    App->>Agent: 启动恢复请求
    Agent->>State: 获取运行锁并读取检查点
    Agent->>Interaction: 读取已提交答案
    Agent->>Runner: 应用答案并继续执行
```

`Agent::answer` 只保存答案并返回带 `ResumeRequest` 的 `RunRequest`。调用方必须再次调用 `start()`。
这个边界使恢复失败可重试，而不重复提交答案。详见 [用户交互](interactions.md)。

## 6. Agent 协作

框架提供两种协作语义。

```mermaid
flowchart LR
    Parent[主运行] -->|agent_spawn| Child[子 Agent<br/>属于父运行树]
    Parent -->|thread_create| Thread[独立 Thread<br/>单独会话]
    Thread -->|thread_send| Thread
    Thread -->|thread_wait / thread_read| Parent
    Thread -->|AwaitingInput| Answer[thread_answer]
    Answer --> Thread
    Thread -->|thread_close| Closed[释放注册表容量]
```

| 语义 | 子 Agent | Thread |
|---|---|---|
| 用途 | 一次性委派 | 后台、长时间或多轮任务 |
| 等待 | `agent_spawn` 同步等待 | 创建后后台运行，使用 `thread_wait` 等待 |
| 身份 | 进入父运行 `RunLineage` | 使用独立会话，不进入父子运行树 |
| 取消 | 继承父运行的取消信号和剩余运行时间 | 不继承发起运行的取消信号和截止时间；`Agent::close` 仍会关闭它 |
| 预算 | 占用父运行预算，由父运行分配预算 | 使用跟新开会话一样的独立预算 |
| 交互 | 没有单独的会话记录，结束后无法后续恢复 | `thread_answer` 恢复原 `run_id` |
| 结果 | 返回完整 `RunResult` | `thread_wait` 返回完整 `ThreadResult` |

### Thread 状态转换

```mermaid
flowchart TD
    Create[thread_create] --> Reserve[预留容量]
    Reserve --> Start[Agent.start]
    Start --> Running[Running]
    Running --> Completed[Completed]
    Running --> Awaiting[AwaitingInput]
    Running --> Limit[StepLimit / ToolCallLimit]
    Running --> Failed[Failed / Cancelled]
    Awaiting -->|thread_answer<br/>原 run_id| Running
    Completed -->|thread_send<br/>新 run_id| Running
    Limit -->|thread_send<br/>新 run_id| Running
    Failed -->|thread_send<br/>新 run_id| Running
    Completed --> Close[thread_close]
    Awaiting --> Close
    Limit --> Close
    Failed --> Close
    Close --> Removed[删除记录<br/>释放容量]
```

默认 Thread 运行时具有以下语义：

- `thread_create` 在 `Agent::start` 前预留容量；启动失败自动退还。
- 同一 Thread 的发消息和回答操作串行执行；活动运行期间拒绝新操作。
- `AwaitingInput` 只允许 `thread_answer`，不允许 `thread_send` 绕过答案结构校验。
- 旧运行只有在自己仍是 `latest_run_id` 时才能更新 Thread 摘要。
- 进程内 Thread 注册表上限为 128，运行结束不自动删除记录；`thread_close` 显式释放容量。
- Thread 注册信息仅存在当前进程。跨进程任务应替换 `AgentCollaborationRuntime`。

子 Agent 和 Thread 结果都保留 `content`、`structured`、`interactions`、`status`、`usage`、`steps` 和
`tool_calls`。详见 [Agent 协作](collaboration.md)。

## 7. 事件与 AG-UI

实时事件使用 `(run_id, EventScope::Live { execution_id }, sequence)` 定位。持久化事件日志使用独立序列。

```mermaid
sequenceDiagram
    participant Producer as Agent / 执行循环 / 调用器
    participant Context as RunContext.publish
    participant Sink as EventSink
    participant Live as RunHandle 实时事件
    participant Observer as ObserverDispatcher
    participant AGUI as AG-UI 事件流

    Producer->>Context: AgentEvent
    opt 诊断事件
        Context->>Sink: on_diagnostic(原始事件)
    end
    Context->>Context: 取得发布锁<br/>封装事件并递增序号
    Context->>Sink: on_published(封装后的事件)
    opt AG-UI sink
        Sink->>AGUI: 非阻塞写入有界通道
    end
    Context->>Live: 发送封装后的事件
    alt 普通事件
        Context->>Observer: 非阻塞写入原始事件
        Observer->>Sink: on_event<br/>超时和代码异常隔离
    else 终态事件
        Context->>Observer: 单独保留终态事件
        Observer->>Sink: 排空普通事件后交付终态事件
    end
    AGUI->>AGUI: 读取时转换为 AG-UI / SSE
```

| 入口 | 语义 |
|---|---|
| `on_diagnostic` | 同步记录上下文组装和模型派发诊断；不保证跨生产者顺序 |
| `on_published` | 按实时发布顺序交接封装后的事件；必须快速，不得进行 I/O 或再次发布 |
| `on_event` | 有界、尽力的异步观测；队列满时可丢失普通事件 |

AG-UI 为每次执行尝试创建独立的有界通道。溢出、超大事件或顺序错误会终止输出流，
但不会取消 Agent。权威结果仍来自 `RunHandle::wait`。

| 协作事件 | AG-UI 映射 |
|---|---|
| 子 Agent 开始/结束/失败 | `SUBAGENT_STARTED / SUBAGENT_FINISHED / SUBAGENT_ERROR` |
| 子 Agent 暂停交互 | `framework.subagent.interactions` CUSTOM |
| Thread 创建/运行/结束/关闭 | `framework.thread.*` CUSTOM |

Thread 不是子运行，因此不映射为 AG-UI `SUBAGENT_*`。协作生命周期事件不包含任务正文、
Thread 消息或用户答案。详见 [AG-UI](agui.md)。

## 8. 资源关闭

```mermaid
flowchart LR
    subgraph Run[运行级]
        Loop[AgentLoop 结束] --> Children[取消子运行]
        Children --> Scope[逆序关闭运行级 ResourceScope]
        Scope --> Hook[on_finished]
        Hook --> Terminal[终态事件与会话登记回收]
    end

    subgraph Owner[Agent 级]
        Close[Agent.close] --> Stop[拒绝新运行<br/>取消所有活动运行]
        Stop --> Drain[等待运行和其他操作清理]
        Drain --> Shutdown[逆序执行插件关闭回调]
        Shutdown --> Cleanup[同步清理 / Drop 兜底]
        Cleanup --> Report[ShutdownReport]
    end
```

`Agent::close` 在所有 Agent 副本之间共享一次关闭。它先等待运行清理，再关闭插件容器。
插件创建的 SQLite 和 MCP 连接在此阶段释放；外部注入的共享组件仍由其所有者管理。

## 实现索引

| 主题 | 主要实现 | 详细文档 |
|---|---|---|
| Agent 组装、运行和 Thread 运行时 | [agent.rs](../crates/framework/eliteforge-agent-framework/src/agent.rs) | [快速上手](quickstart.md) |
| ReAct / 工作流状态机 | [model_execution.rs](../crates/framework/eliteforge-agent-framework/src/model_execution.rs) | [执行约束](execution-contracts.md) |
| DAG 调度 | [dag.rs](../crates/framework/eliteforge-agent-framework/src/dag.rs) | [DAG 工作流](dag-workflows.md) |
| 模型选择与调用 | [models.rs](../crates/framework/eliteforge-agent-framework/src/models.rs) | [模型插件](model-plugins.md) |
| 工具/模型中间件与 `ToolInvoker` | [middleware.rs](../crates/framework/eliteforge-agent-core/src/middleware.rs) | [中间件与钩子](extensions.md) |
| 交互契约 | [interaction.rs](../crates/framework/eliteforge-agent-core/src/interaction.rs) | [用户交互](interactions.md) |
| 协作契约 | [collaboration.rs](../crates/framework/eliteforge-agent-core/src/collaboration.rs) | [Agent 协作](collaboration.md) |
| 事件发布与观测 | [types.rs](../crates/framework/eliteforge-agent-core/src/types.rs)、[observers.rs](../crates/framework/eliteforge-agent-core/src/observers.rs) | [可观测](observability.md) |
| AG-UI 转换 | [adapter.rs](../crates/plugin/eliteforge-agent-official-plugin-agui/src/adapter.rs) | [AG-UI](agui.md) |
