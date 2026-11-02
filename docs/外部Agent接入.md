# 外部 Agent 接入

外部 Agent 不是 `ChatModel`。它自行运行决策循环、调用工具、处理审批并管理会话。Framework 只负责本地运行准入、取消与超时、事件转发，以及返回文本和外部会话引用。

```mermaid
flowchart LR
    Host[宿主应用] --> Framework[ExternalAgentLoop]
    Framework --> Adapter[ExternalAgent 适配器]
    Adapter --> External[CLI 或 JSON 服务]
    External --> Adapter
    Adapter --> Events[文本／状态／工具／审批观察事件]
    Adapter --> Session[外部 session／run 引用]
```

`external_agent_pack(adapter)` 装配 `ExternalAgentPlugin` 和 `ExternalAgentLoop`。这个执行循环不要求 `ConversationStore` 或 `StateStore`；Framework 不执行外部工具、不保存外部对话，也不写外部 checkpoint。请求通过 `ExternalSessionRef` 传入已有外部会话；结果的 `RunResult.external_session` 返回外部引用。`resume` 不适用于这个循环，调用时明确拒绝。宿主需要保存外部引用，并由外部系统承担恢复语义。

官方适配器位于 `eliteforge-agent-official-plugin-agent-external`，提供 Codex／OpenCode／dsh CLI 传输和可配置的 JSON Bridge。工具调用、审批和状态作为观察事件发布，不会进入 Framework 的 ToolProvider 或审批执行链。输出事件不等于外部操作成功的确认，最终状态以适配器结果为准。

外部运行通过标准 `OutputEvent` 发布带唯一消息 ID 的文本和思考增量，因此 AG-UI 不必等待最终结果才显示内容。正常结束发布 `MessageFinished`；失败、取消、超时或执行 future 被丢弃时，清理守卫发布 `MessageAborted`。最终文本包含已输出前缀时只补齐后缀；过程文本与最终答复不同时保留两条消息，避免重复前缀或文本校验失败。

状态、工具、审批和 session 既保留原始观察事件，也通过 `framework.external_agent` 自定义输出事件交给 AG-UI。这些事件不会触发 Framework 的工具执行或审批流程。

Codex 新任务使用 `exec --cd`；续聊使用 `exec resume`，不传该子命令不支持的 `--cd`。两种方式都通过进程工作目录设置执行位置，外部 session 引用原样传递。

取消 CLI 运行时会清理本地进程树。JSON Bridge 在新任务执行中拿不到远端 run ID，因此取消只能保证本地不再等待；远端状态仍未知。只有宿主事先提供了远端 run ID，适配器才能尽力请求远端取消，HTTP 响应也不能当作远端已停止的证明。超时遵守同一边界。

## DeepSeek Harness（dsh）

`ExternalCliKind::Dsh` 由 `eliteforge-agent-official-plugin-agent-external` 实现，通过
`eliteforge-agent-capability-external` 重导出。`DshCliOptions` 控制是否恢复保存凭据；凭据内容由 dsh 读取。
供应商路由保存在各自 profile 中，恢复凭据不会自动读取桌面版的模型配置。
设置 `provider_profile: Some("desktop".into())` 后，适配器只从该 profile 的 `llm-pi-ai`
条目读取模型路由，合并 headless 和 home 层的同名路由覆盖，写入本次调用的临时 overlay。
桌面 UI、工具、权限、默认模型均不复制，也不改写原 profile；发现和执行使用同一套配置。
`DshModelProbe::discover` 在 20 秒内通过内嵌的 `eliteforge-dsh-model-catalog-probe` Cordis 插件读取目录，
不创建对话；同步宿主可使用 `discover_blocking`。探测返回模型和部分 provider 的错误。

宿主通过 `external_agent_pack_with_loop(adapter, loop_id)` 自定义循环插件 ID，默认入口仍使用
`framework.loop`。能力包 ID 和外部绑定插件 ID 均为 `framework.external-agent`，属于不同命名空间。

```mermaid
flowchart LR
    Host[Tron 或其他宿主] --> Pack[capability-external：选择与装配]
    Pack --> Loop[Framework ExternalAgentLoop]
    Pack --> Plugin[agent-external：CLI 适配]
    Loop --> Plugin
    Plugin --> DSH[dsh headless 子进程]
```

宿主保存原生 session 引用；dsh 自行管理工具、权限和会话。调用使用 stdin 传任务，临时 overlay
选择 provider/model、推理强度并按选项恢复保存凭据。进程取消时清理进程树和临时目录。
JSON 模式的思考、工具和状态按步骤转发，正文使用成功运行的无损 `final.text`，中间的截断正文不拼入答案。
