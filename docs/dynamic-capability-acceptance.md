# 动态能力包验收矩阵

本次实现覆盖能力包管理、独立进程、现有扩展点、空闲重配、SDK 和中立开发宿主；不接入 Tron。设计及公共 API 见 [运行时方案](dynamic-capability-runtime.md)，破坏性变更见 [迁移说明](dynamic-capability-migration.md)。

## 扩展覆盖

`crates/tools/eliteforge-agent-capability-dev-host/tests/sidecar.rs` 打包并启动真正的 Rust 后端，通过 stdio 协议调用宿主现有接口。测试模型全部离线，不调用付费供应商。

| 分类 | 契约 | 端到端行为 |
|---|---|---|
| 执行 | AgentLoop | 远程循环反向调用模型、跨包自定义实现；声明依赖检查；远程状态创建、租约、提交；持久化问题和答案恢复 |
| 执行 | WorkflowNode | 原生 DAG 调用远程节点；回调节点快照；节点状态与会话原子写入；节点中间件 |
| 模型 | ChatModel、ModelDiscovery、ModelSelectionStrategy、TokenEstimator | 真正流回调、握手目录、远程选择与异步估算；模型反向 Host API 和依赖调用；W3C 追踪上下文 |
| 模型 | Embedding、Rerank、Image | 向量及排名校验、图像结果；超过 8 MiB 的实际请求经过二进制分块 |
| 模型 | Video | 提交、查询、取消；未结束任务阻止重配；序列化句柄绑定原实例，取消后拒绝再用 |
| 模型 | SpeechRecognition、SpeechSynthesis、Realtime | 媒体读写；实时会话消息、结果、Busy 占用和关闭 |
| 工具 | ToolProvider、ToolPolicy | 远程工具租约、调用、策略；原生执行预算和 schema 验证；输入请求与恢复 |
| 上下文 | ContextProvider、ContextContributor、ContextAssembler、ContextBudgetPolicy、ContextReducer | 原生上下文装配链消费远程结果；按块压缩；独立选择装配器，避免单实现冲突 |
| 上下文 | SkillProvider、LongTermMemory | Skills 目录、加载、资源、释放；原生 progressive 工具；远程 SQLite 记忆读写 |
| 存储 | ConversationStore、StateStore、InteractionStore | 快照、原子会话提交、检查点 CAS、租约排他和释放、持久化交互、重启恢复 |
| 存储 | SummaryStore、ArtifactStore | 摘要 CAS 冲突、版本删除；附件不可变发布、范围与取消、类型化 NotFound；重启保留数据 |
| 中间件 | Run、Model、Tool、WorkflowNode | 宿主保留后续链，远程前后处理；`next` 一次性，重复调用拒绝 |
| 生命周期 | RunLifecycleHook | started／finished 回调；资源关闭报告 |
| 输出与观测 | RunOutputFactory、EventSink | 强类型输出宿主适配；输出流占用；显式事件订阅；观测回调堵塞不阻塞任务执行 |
| 自定义 | 稳定版本通信契约 | 按实现 ID 路由，跨包双向嵌套调用；包依赖和实现依赖都校验；嵌套回调共享调用预算；`Any` 不跨进程 |

Core 的全部扩展点及 Context 的两个存储扩展均有原生代理；TokenEstimator 额外进入版本化远程契约。示例清单提供 35 个实现，包含一个自定义契约，不能一次启用互斥的上下文实现。

## 运行时与迁移

| 验收项 | 证据 |
|---|---|
| PluginPack 移除，静态组合迁移 | Framework `capability_packs`／`official_capability_composition`；官方能力包测试；原有工作区回归 |
| 任意组合、选择隔离、依赖解析 | 清单测试、签名安装、求解器回溯；MCP 未选中不创建进程；插件／页面精确选择传入 SDK；静态 Skills＋资源包通过原生 SkillsRuntime，未选中的 Skills 不加载，且不启动后端 |
| 独立作用域与进程复用 | 同一 Agent 多实现一个 sidecar；不同 Agent 经本地父能力包递归装配仍独立；配置、物理实例和数据作用域分离 |
| 不可变版本、签名和完整性 | Runtime 安装测试、打包签名测试、ZIP 路径攻击和篡改拒绝；跨管理器的版本文件锁 |
| 空闲重配、Busy 和恢复 | Framework `reconfigure.rs`；页面／模型会话／视频／输出／等待任务 Busy；候选失败恢复；直接插件不重复 install／ready |
| 待恢复任务版本绑定 | 远程工具等待后关闭宿主；新管理器重开仍阻止删除；错误版本恢复失败；原版本完成后解除 pin |
| 双向调用与取消 | Runtime 嵌套 RPC；回调取消、连接关闭；取消抢先到达拒绝迟到调用；超时及崩溃结果未知；不自动重放 |
| 控制与大载荷隔离 | 超过 8 MiB 的 RPC 双向传输；64 KiB 分块、有界组装；真实 sidecar 图像请求、附件上传；结果／取消优先队列 |
| 子进程清理 | 实际 MCP stdio 服务 PID 检查；能力实例关闭回收 MCP 子进程；SDK shutdown 先清理资源与租约，再超时终止进程组 |
| 独立页面和宿主挂载 | `sdk/page/tests/browser.mjs`；真实 Rust 宿主、iframe、MessagePort、页面 SSE、主题／语言、Busy、开发重新挂载、关闭清理和旧会话拒绝 |
| 能力包与扩展点视图 | 示例宿主按版本、定义方、来源、单／多规则、分类、实际 Agent 选择和占用显示 |
| 开发构建与正式协议 | `scripts/capability-dev.mjs --once` 实际构建、打包、安装和重配；先完整构建再发布，失败构建不进入安装／切换阶段；失败前后版本、运行实例及刷新序号不变，旧 Agent 仍正常执行 |

## 验证范围

本地验证环境为 macOS arm64、Rust 1.95.0、Node 26、Chromium。2026-09-26 最终本地结果：

| 检查 | 结果 |
|---|---|
| 完整工作区 `cargo test --workspace --all-targets` | 384 项通过，74 个测试套件 |
| Framework `--no-default-features --all-targets` | 135 项通过，17 个测试套件 |
| 工作区文档测试 | 无失败；原有 1 项 ignore |
| 格式、Clippy `-D warnings`、Rustdoc `-D warnings` | 通过 |
| 页面 SDK 构建和开发脚本语法 | 通过 |
| 真实浏览器与 Rust 宿主 | iframe 调用／主题、语言、Busy、开发重新挂载、迟到隔离、关闭清理、分类目录均通过；保留活跃 SSE 页面退出宿主也无需强制终止 |
| 开发工具成功与失败构建 | 成功构建发布并切换；故意失败的构建不改变版本、实例与刷新序号，旧 Agent 仍可执行 |

五个实施阶段的本地交付已落地；跨平台验收仍待下面的工作流实际执行，不能将本地结果等同于完整验收。

`.github/workflows/capability-runtime.yml` 提供 macOS、Windows、Linux 的真实进程测试和 Linux 浏览器测试。当前环境没有 Windows／Linux 执行主机，仓库也未配置 Git remote，尚未在这两个系统执行 CI。不能将 CI 配置存在当成三平台验收通过；完整平台验收需要该工作流实际成功。

## 使用边界

- 安装版本、Agent 选择和运行实例独立；安装不切换已有 Agent。业务数据不会因代码回滚而回退。
- 签名信任来自宿主配置；开发安装须显式开启。首版没有在线商店。
- 空闲重配需要释放所有占用；不强制结束任务，不同时服务两个版本。恢复失败会明确标记不可用。
- 持久化版本 pin 先于检查点写入保护版本。结果未知的写入可能留下保守 pin，宿主需核对执行状态后显式修复；不能自动删除保护来伪装成功。
- 跨进程摘要／附件是存储契约，不包含业务数据迁移。附件首版上限 32 MiB，视频句柄每实例最多 256。
- 观测订阅队列有界，拥塞可以丢弃并计数；业务正文溢出明确失败，不静默丢弃。
