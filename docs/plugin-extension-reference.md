# 插件扩展点参考

这是一份查阅表。第一次开发插件，请先完成[插件开发指南](plugins.md)中的依赖配置和入门示例。
每个表都回答：实现什么接口、怎么注册、什么时候调用，以及能否配置多个。

业务能力由接口实现提供。`ContextPlugin`、`ToolProviderPlugin` 等是现成的注册封装，
用于把实现接入当前安装流程；它们不会额外提供上下文或工具业务逻辑。
能力包自动装配已经可用：用 Core 导出的 `#[capability(...)]` 标记支持的 trait `impl`，
应用选择 `CapabilityPack` 后由框架创建并适配实例。手工注册封装和 `PluginPack` 仍完全兼容。

## 能力包自动装配

业务 crate 使用 `eliteforge_agent_core::{capability, CapabilityPackBuilder}`。属性标在 trait
`impl` 上；与 `async_trait` 共用时，`#[capability]` 放在外层。

~~~rust
#[capability(
    pack = "shop",
    id = "shop.context",
    instance = "shop.context",
    depends_on = ["shop.catalog"],
    config = ShopConfig,
    factory = ShopContext::from_config
)]
#[async_trait]
impl ContextProvider for ShopContext {
    // ...
}
~~~

`id`、`instance` 和 `depends_on` 都可省略；前两项生成稳定默认值，依赖默认为空。
`config` 和 `factory` 必须同时出现，factory 签名是 `fn(&Config) -> Self`。两者都省略时，
实现类型必须满足 `Default`。应用选择包及显式子包：

~~~rust
let agent = AgentBuilder::new()
    .capability_pack(
        CapabilityPackBuilder::new("shop", shop_config)
            .include(CapabilityPackBuilder::new("shop.audit", audit_config)),
    )
    .build()
    .await?;
~~~

解析器在 factory 前检查包/能力 ID、依赖、环和共享实例声明，按稳定拓扑顺序创建对象。
预检包含应用传入的普通插件依赖边。未选包不运行 factory；同一 `instance` 的多接口声明共享对象，
省略时按包和实际类型身份共享，支持跨模块和类型别名。不同 Agent 的对象与生命周期隔离。
首版不支持异步或依赖服务 factory、成员开关和官方包迁移。详情见
[能力包与自动装配](capability-packs.md)。

首版支持的标记目标是：`ContextProvider`、`ToolProvider`、`EventSink`、
`ContextContributor`、`ModelMiddleware`、`ToolMiddleware`、`RunMiddleware`、
`ToolPolicy`、`RunLifecycleHook`、`WorkflowNode`、`WorkflowNodeMiddleware`、
`RunOutputFactory` 和 `Plugin`。其他 trait 继续用对应注册封装或自定义 Plugin。

## 怎样使用下面的注册类型

除特别标注外，接口、注册适配器和服务类型都从 `eliteforge_agent_core` 引入。
应用也可从 `eliteforge_agent_framework` 引入这些重新导出的类型。

下面两种是手工 Plugin 路径，注册同一种能力：

```rust
// 应用装配：使用已实现 Plugin 的适配器。
let builder = builder.plugin(ContextPlugin::new("shop.context", implementation));

// 自定义 Plugin::install 内：直接注册框架认识的服务类型。
context.contribute(ContextService(Arc::new(implementation)))?;
```

这是两个独立片段；`implementation` 都是实现了 `ContextProvider` 的对象。
`provide` 注册唯一服务，`contribute` 注册多个贡献。多实例仍要求插件 ID 唯一。
具体类型是注册键：`ContextService`、`Arc<ContextService>` 和你的实现类型是三个不同的键。

## 装配

| 接口或入口 | 你需要实现或提供什么 | 框架什么时候使用 | 数量与约束 |
|---|---|---|---|
| `#[capability(...)]` | 支持的 Core trait `impl`，以及可选 ID/实例/依赖和可选成对 config/factory | 业务 crate 链接时登记描述；仅选中包在 build 时实例化 | 描述静态；factory 同步；同实例的声明必须兼容 |
| `CapabilityPackBuilder::new(id, config)` | 当前 Agent 的包 ID 和配置；用 `.include(child)` 选择子包 | `.capability_pack(pack)` 时保存选择 | 子包必须显式加入；重复包/包含环报错 |
| `CapabilityPack` | 自定义包 ID、配置和显式子包闭包 | Agent build 前解析 | 每个选中 Agent 独立创建业务对象 |
| `Plugin` | `id()`、`install(&mut PluginContext)`；需要前置插件时实现 `dependencies()` | `AgentBuilder::build` 时安装 | ID 在一个 Agent 内唯一 |
| `PluginPack` | `into_plugins(self)` 返回已有 Plugin 安装配置；可全部是 Provider 的注册封装 | `.plugin_pack(pack)` 时展开 | 兼容 API；可以多个、可以嵌套；官方包继续使用 |
| `PluginContext::provide<T>` | 唯一角色的服务对象 | 安装期间写入容器 | 同类型一个 |
| `PluginContext::contribute<T>` | 某类能力的一个贡献 | 安装期间写入容器 | 同类型多个，保留注册顺序 |
| `PluginContext::services` | 用 `get<T>` 或 `all<T>` 解析已有服务 | 其他插件的 `install` 中 | `dependencies` 负责保证安装顺序 |
| `on_shutdown` / `on_cleanup` | 异步关闭 / 同步兜底回调 | Agent 关闭或安装失败、取消、Drop | 按资源回调注册的逆序处理 |

来源：[Plugin 与容器](../crates/eliteforge-agent-plugin-kernel/src/container.rs)、
[服务注册表](../crates/eliteforge-agent-plugin-kernel/src/services.rs)。

## 上下文

适用场景：系统指令、RAG、长期记忆注入、历史裁剪和摘要压缩。

| 接口与主要方法 | 注册适配器 → 服务类型 | 调用时机 | 多个实现及注意事项 |
|---|---|---|---|
| `ContextProvider::context` | `ContextPlugin` → `contribute(ContextService)` | 内置执行器准备一轮模型指令时 | 多个结果合并；只返回 System/Developer 指令 |
| `ContextContributor::block_id / contribute` | `ContextContributorPlugin` → `contribute(ContextContributorService)` | 默认组装器准备自定义上下文块时 | 多个来源；块 ID 唯一，必须配套显式布局和配额 |
| `ContextLayout` 配置 | `ContextLayoutPlugin` → `provide(ContextLayoutService)` | 构建时校验，组装时决定顺序与配额 | 一个布局，包含指令、历史和每个自定义来源 |
| `ContextBudgetPolicy::allocate` | `ContextBudgetPolicyPlugin` → `provide(ContextBudgetPolicyService)` | 组装器为各块分配预算时 | 一个全局策略 |
| `ContextReducer::reduce` | `ContextReducerPlugin` → `provide(ContextReducerService)`；或 `ContextBlockConfig::reduce_with` | 对应块需要裁剪/压缩时 | 全局历史 reducer 一个；也可按块配置 |
| `ContextAssembler::assemble` | `ContextAssemblerPlugin` → `provide(ContextAssemblerService)` | 每次准备 Chat 模型输入时 | 一个；整体接管来源、布局、预算与裁剪 |

安装自定义 `ContextAssembler` 时，单独配置的布局、贡献来源、预算和 reducer 会冲突；
这些配置应由自定义组装器持有。显式布局中的历史块使用块自己的 reducer，不能再另装全局历史 reducer。

来源：[上下文接口](../crates/eliteforge-agent-core/src/context.rs)、[上下文使用指南](context-windows.md)。

## 工具与交互

| 接口与主要方法 | 注册方法 | 调用时机 | 多个实现及注意事项 |
|---|---|---|---|
| `Tool::call(arguments, context)` | 与 `ToolDescriptor` 组成 `ToolBinding`，放进工具来源 | 模型请求调用，并通过框架校验后 | 多个工具；内部 `id` 和模型 `call_name` 都要避免冲突 |
| `ToolProvider::lease(context)` | `ToolProviderPlugin` → `contribute(ToolProviderService)` | 内置执行器为本次运行取得工具时 | 多个来源合并；动态可见性在这里决定 |
| `ToolPolicy::evaluate(invocation)` | `ToolPolicyPlugin` → `contribute(ToolPolicyService)` | 工具调用授权和审批决策时 | 多个策略参与校验；自定义循环必须支持并执行策略 |
| `InteractionRequest` + `ToolOutput::request_input` | 由工具返回，配合 `InteractionStore` | 需要用户回答或批准时 | 运行进入等待输入状态；恢复由 `agent.answer` 后再次运行 |

固定工具集合使用 `StaticToolProvider`。`ToolLease` 提供本次运行内的同步清理和异步关闭入口。
不要在插件 `install` 阶段执行某个订单查询或业务写操作；此时只注册能力。

来源：[工具接口](../crates/eliteforge-agent-core/src/types.rs)、[工具策略](../crates/eliteforge-agent-core/src/state.rs)、
[问答与审批](interactions.md)。

## 中间件、通知与输出

| 接口与主要方法 | 注册适配器 → 服务类型 | 调用时机 | 多个实现及注意事项 |
|---|---|---|---|
| `RunMiddleware::intercept` | `RunMiddlewarePlugin` → `contribute(RunMiddlewareService)` | 一次 Agent 运行的执行链 | 多个嵌套；`next.run()` 继续执行 |
| `WorkflowNodeMiddleware::intercept` | `WorkflowNodeMiddlewarePlugin` → `contribute(WorkflowNodeMiddlewareService)` | 一次工作流节点执行 | 多个嵌套；`next.run(input, context)` |
| `ModelMiddleware::intercept` | `ModelMiddlewarePlugin` → `contribute(ModelMiddlewareService)` | 一次 Chat 模型调用，包括适用的重试/辅助调用 | 多个嵌套；`next.run(request, context)`；不拦截独立 Embedding 等操作 |
| `ToolMiddleware::intercept` | `ToolMiddlewarePlugin` → `contribute(ToolMiddlewareService)` | 一次已选定工具的调用 | 多个嵌套；`next.run()`；参数与目标不可替换 |
| `RunLifecycleHook::on_started / on_finished` | `RunLifecycleHookPlugin` → `contribute(RunLifecycleHookService)` | 运行准入后的开始/结束通知 | 多个，按贡献顺序通知；失败隔离，不改变结果 |
| `EventSink::on_event` | `EventPlugin` → `contribute(EventService)` | 有界观测队列消费事件时 | 多个消费者；回调完成与事件交付不是可靠性保证 |
| `EventSink::on_published` | 同上 | 普通异步观测入队前的同步发布路径 | 快速、无阻塞、无 I/O；适合把事件交给有界通道 |
| `EventSink::on_diagnostic` | 同上 | 上下文报告和最终模型派发诊断时 | 同步、不可阻塞；适合在关联 span 存活时记录指标 |
| `RunOutputFactory::output_type / prepare` | 自定义插件 `contribute(RunOutputService)` | `agent.start` 发布首个运行事件之前 | 不同输出类型可以共存；同一类型只能一个工厂，每次输出只能取一次 |

中间件根据贡献顺序形成外层到内层的链，插件依赖会影响该顺序。
Hook 和事件用于通知；资源关闭使用容器回调、`ToolLease` 或会话租约。

来源：[Middleware](../crates/eliteforge-agent-core/src/middleware.rs)、[Hook](../crates/eliteforge-agent-core/src/hooks.rs)、
[事件接口](../crates/eliteforge-agent-core/src/types.rs)、[输出工厂](../crates/eliteforge-agent-core/src/operations.rs)。
语义与示例见[Middleware 与 Hook](extensions.md)和[AGUI](agui.md)。

## 模型

适用场景：接入模型协议、增加能力、按请求选择模型。已有 Genai 协议优先使用配置，详见[模型插件](model-plugins.md)。

| 接口与主要方法 | 注册方法 | 由谁触发 |
|---|---|---|
| `ChatModel::generate` | `ChatModelPlugin` / `ModelConnectionPlugin` → `contribute(ModelConnectionService)` | Agent 循环的 Chat 执行器 |
| `EmbeddingModel::embed` | `contribute(EmbeddingModelService::new(model_ref, adapter))` | `agent.embed / embed_query` 或节点中的模型执行服务 |
| `RerankModel::rerank` | `contribute(RerankModelService::new(...))` | `agent.rerank` |
| `ImageGenerationModel::generate` | `contribute(ImageGenerationModelService::new(...))` | `agent.generate_image` |
| `VideoGenerationModel::submit / query / cancel` | `contribute(VideoGenerationModelService::new(...))` | `agent.submit_video / query_video / cancel_video` |
| `SpeechRecognitionModel::transcribe` | `contribute(SpeechRecognitionModelService::new(...))` | `agent.transcribe` |
| `SpeechSynthesisModel::synthesize` | `contribute(SpeechSynthesisModelService::new(...))` | `agent.synthesize` |
| `RealtimeModel::connect` + `RealtimeSession` | `contribute(RealtimeModelService::new(...))` | Agent 的实时连接、发送、接收和关闭接口 |
| `ModelSelectionStrategy::select` | `ModelStrategyPlugin` → `provide(ModelStrategyService)` | 未被请求显式模型指定覆盖的 Chat 选择过程 |
| `ModelRouting` 配置 | `ModelRoutingPlugin` → `provide(ModelRouting)` | 默认模型和阶段模型选择 |
| `TokenEstimator` | `ModelConnection::with_token_estimator` | 对应连接的上下文预算估算 |

模型能力可以注册多个，按连接、模型 ID 和操作区分；重复绑定会失败。
Chat 连接名必须唯一，多个 Chat 模型放进连接的模型列表。
静态 `ModelRouting` 与自定义 Chat 策略同时存在时，自定义策略优先；独立模型操作使用自己的路由。

`ModelDiscovery` 是适配器可选的目录查询接口。发现结果需要经过筛选并形成显式模型注册，
它不会因为对象实现了这个 trait 就自动加入 Agent 的模型目录。

来源：[Chat 与模型选择](../crates/eliteforge-agent-core/src/models.rs)、[多模态能力与服务](../crates/eliteforge-agent-core/src/capabilities.rs)、
[TokenEstimator](../crates/eliteforge-agent-core/src/tokens.rs)。

## 存储

存储按数据职责拆分；每个角色在一个 Agent 内是唯一服务，可以分别由不同插件提供。

| 接口与主要职责 | 注册适配器 → 服务类型 | 何时调用 |
|---|---|---|
| `ConversationStore`：打开会话或分支 | `ConversationStorePlugin` → `provide(ConversationStoreService)` | 运行读取/保存消息；`agent.conversation / append_conversation` |
| `ConversationSession`：快照、带版本追加、结束租约 | 从 `ConversationStore::open/open_branch` 返回 | 一次会话读写作用域内；不独立注册插件 |
| `StateStore`：checkpoint、journal、租约、原子状态迁移 | `StateStorePlugin` → `provide(StateStoreService)` | 持久化执行、恢复与 checkpoint 查询 |
| `InteractionStore`：问题、审批、答案、恢复记录 | `InteractionStorePlugin` → `provide(InteractionStoreService)` | 暂停、`agent.pending_interactions / interaction / answer` |
| `LongTermMemory`：检索、读写与版本控制 | `LongTermMemoryPlugin` → `provide(LongTermMemoryService)` | `agent.memory / search_memory / save_memory / delete_memory` 或上下文检索组件 |
| `SummaryStore`：摘要读取、比较交换、删除 | 自定义插件 → `provide(SummaryStoreService)` | 摘要组件与 `agent.execute(LoadSummary / SaveSummary / DeleteSummary)` |
| `ArtifactStore`：附件大小限制、原子写入、分块读取、删除 | 自定义插件 → `provide(ArtifactStoreService)` | 附件组件与 `agent.execute(PutArtifact / ReadArtifact / RemoveArtifact)` |

前四类存储接口来自 Core。`SummaryStore`、`ArtifactStore` 来自 `eliteforge-agent-context`；
目前它们的 `SummaryStoreService`、`ArtifactStoreService` 和 Agent 操作请求类型由
`eliteforge-agent-official-plugin-storage-sqlite` 导出，使用这些现成入口要声明该 crate 的依赖。
服务内部可以装自定义实现；也可定义自己的服务类型和 `AgentOperation`。

SQLite 官方存储插件一次注册这些唯一角色。官方包的 `.storage(...)` 当前只接收 SQLite 插件配置；
替换为其他数据库时，按所需角色单独装配或使用自己的包。

来源：[会话](../crates/eliteforge-agent-core/src/conversation.rs)、[执行状态](../crates/eliteforge-agent-core/src/state.rs)、
[交互](../crates/eliteforge-agent-core/src/interaction.rs)、[长期记忆](../crates/eliteforge-agent-core/src/long_term.rs)、
[摘要](../crates/eliteforge-agent-context/src/summary.rs)、[附件](../crates/eliteforge-agent-context/src/artifact.rs)。

## 工作流与应用主动操作

| 接口与主要方法 | 注册方法 | 调用时机 | 数量与约束 |
|---|---|---|---|
| `WorkflowNode::kind / configuration_schema / validate / execute` | `WorkflowNodePlugin` → `contribute(WorkflowNodeService)` | DAG 引用并调度节点时 | 多个节点类型；`kind` 唯一 |
| `AgentLoop::run` 与能力声明方法 | `AgentLoopPlugin` → `provide(LoopService)` | Agent 执行整个运行策略 | 一个循环；自定义实现负责遵守已声明的能力契约 |
| `AgentOperation::Service / Output / execute` | 自定义插件注册它依赖的服务；请求类型本身不注册 | `agent.execute(request)` | 默认只接受一个匹配服务；可实现 `select` 选择多个候选中的一个 |

`AgentOperation` 适合应用主动查询或调用业务服务。它使用 Agent 的取消和关闭控制，
不会自动进入模型工具的策略或中间件链。

来源：[WorkflowNode](../crates/eliteforge-agent-core/src/dag.rs)、[AgentLoop](../crates/eliteforge-agent-core/src/types.rs)、
[AgentOperation](../crates/eliteforge-agent-core/src/operations.rs)。

## 应用操作入口速查

插件注册完成后，上层应用通过 Agent 实例操作：

| 需求 | Agent 方法 |
|---|---|
| 启动、等待、直接运行 | `start`、`wait`、`run` |
| 获取控制标识、取消、追加指令 | `control`、`cancel`、`steer` |
| 撤销工具、查看预算 | `revoke_tools`、`budget_usage` |
| 原始事件与自定义输出 | `events`、`subscribe`、`output::<T>`、`start_with_output::<T>` |
| 查看模型、设置独立模型操作选项 | `models(operation)`、`with_model_options(options)` |
| 用户问答 | `pending_interactions`、`interaction`、`answer` |
| 会话与执行状态 | `conversation`、`append_conversation`、`checkpoint`、`latest_checkpoint` |
| 长期记忆 | `memory`、`search_memory`、`save_memory`、`delete_memory` |
| 自定义插件操作 | `execute(request)` |
| 关闭资源 | `close().await` |

`RunHandle` 和 `RunControl` 保存运行及控制标识，操作它们仍调用 Agent。
同一 Agent 的克隆可以操作同一运行；独立构建的其他 Agent 会拒绝。
输出流取得后由应用读取数据。模型的其他操作见上面的模型表。
