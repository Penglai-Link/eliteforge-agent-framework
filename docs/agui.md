# AG-UI 事件

`eliteforge-agent-official-plugin-agui` 将 Framework 的运行输出转换为 AG-UI 事件，并提供 SSE 编码。
ReAct、串行 Workflow 和 DAG 使用同一个插件。

```text
ModelInvoker / Loop / 自定义节点
              │ OutputEvent + 节点执行身份
              ▼
     EventSink.on_published
              │ 按发布顺序写入有界通道
              ▼
         AgUiPlugin
              │ 标准 AG-UI 事件 / SSE 帧
              ▼
     应用 HTTP 接口 → 前端
```

Framework 提供通用输出事件及其顺序；`eliteforge-agent-official-plugin-agui` 负责协议类型、消息配对和 SSE 编码。
HTTP 路由、认证、权限、浏览器连接和持久化回放由应用负责。

实现参考了 Tron 的 `agui_translate.rs` 和 `agui_stream.rs`，协议格式以
[AG-UI 官方事件说明](https://docs.ag-ui.com/concepts/events)为准。
插件复用 MIT 许可的社区 [ag-ui 0.3.0 SDK](https://docs.rs/ag-ui/0.3.0/ag_ui/)，
仅启用协议类型与 SSE 编码；没有引入它的服务端执行器。
该 SDK 目前未获 AG-UI 官方组织认可。Tron 代码未修改。

## 注册与使用

```toml
[dependencies]
eliteforge-agent-official-plugin-agui = { path = "../eliteforge-agent-framework/crates/eliteforge-agent-official-plugin-agui" }
```

在已配置模型和 SQLite 存储的 AgentBuilder 上注册插件，完整配置见[用户问答与工具审批](interactions.md)：

插件注册唯一安装标记及 EventService。启动前通过 `Agent::service<T>()` 检查标记，
未安装或使用了另一个实例时直接拒绝，不启动模型或工具。

```rust
use eliteforge_agent_official_plugin_agui::AgUiPlugin;
use eliteforge_agent_framework::RunRequest;

let agui = AgUiPlugin::default();
let agent = builder.plugin(agui.clone()).build().await?;

// 使用安装时的同一实例或它的 clone 启动。
let (handle, mut events) =
    agui.start(&agent, RunRequest::new("chat-1", "分析项目"))?;
let control = handle.control();

let consume = async {
    while let Some(frame) = events.recv_sse().await? {
        // HTTP handler 将 frame 写入 text/event-stream 响应体。
        print!("{frame}");
    }
    Ok::<(), eliteforge_agent_official_plugin_agui::ag_ui::Error>(())
};
let (result, delivered) = tokio::join!(handle.wait(), consume);
delivered?;
let result = result?;
```

`events.recv()` 返回 SDK 的 `Event`，`events.recv_sse()` 返回编码后的 SSE 帧。
消费方应识别 `RUN_ERROR`；`recv_sse()` 的 `Err` 只表示编码失败。
SSE 内容直接使用 AG-UI JSON，不再套一层应用的普通 JSON 响应体：

```text
data: {"type":"TEXT_MESSAGE_CONTENT","messageId":"...","delta":"你好"}

```

`control` 可以交给接收用户消息的任务，继续调用 `steer()`、`cancel()` 或 `revoke_tools()`。
保留控制句柄不会阻止事件流结束。丢弃事件接收器只停止消费；丢弃 `RunHandle` 或它的
等待 future 会取消执行。HTTP 断开时是否取消 Agent，由宿主决定。

可直接运行不需要模型账号的完整示例：

```sh
cargo run -p eliteforge-agent-official-plugin-agui --example sse
```

源码：[sse.rs](../crates/eliteforge-agent-official-plugin-agui/examples/sse.rs)。服务端复用 Agent，退出时调用 `agent.close()`。

## 事件映射

| Framework 输出 | AG-UI 事件 | 说明 |
|---|---|---|
| RunStarted | RUN_STARTED | `threadId` 使用 session_id；`runId` 使用本次 executionId，恢复时生成新值 |
| 模型调用、DAG 节点、显式步骤 | STEP_STARTED / STEP_FINISHED | 步骤结束表示执行已结束，不等同于成功 |
| MessageStarted / TextDelta / MessageFinished | TEXT_MESSAGE_START / CONTENT / END | 第一段文本到达时打开消息；最终回复补齐未流式返回的后缀 |
| ReasoningDelta | REASONING_START、REASONING_MESSAGE_START / CONTENT / END、REASONING_END | 只发送模型公开返回的推理文本；可配置关闭 |
| 模型返回的 tool_calls | TOOL_CALL_START / ARGS / END | 参数作为完整 JSON 发送；END 表示参数发送结束，结果随后到达 |
| ToolResult | TOOL_CALL_RESULT | 保留 content、structured、isError、effect 和 resultReference |
| StateSnapshot / StateDelta | STATE_SNAPSHOT / STATE_DELTA | 应用显式发送客户端状态；增量采用 RFC 6902 |
| ActivitySnapshot / ActivityDelta | ACTIVITY_SNAPSHOT / ACTIVITY_DELTA | 活动 ID、类型与快照配对，增量字段为 `patch` |
| Custom | CUSTOM | 应用自行约定 name/value |
| Steered | CUSTOM: framework.steered | 指令已写入历史，不表示模型已处理 |
| 非文本内容、结构化模型回复 | CUSTOM: framework.message | 保留原有内容类型及 structured，不强行转换成回答文本 |
| RunFinished: Completed | RUN_FINISHED，success | result 带最终 RunResult |
| AwaitingInput / RecoveryRequired | RUN_FINISHED，interrupt | 分别等待用户输入或人工核对执行结果 |
| StepLimit / ToolCallLimit | RUN_ERROR | code 为 AGENT_LIMIT_REACHED |
| RunCancelled / RunFailed | RUN_ERROR | 分别为 AGENT_CANCELLED、AGENT_RUN_FAILED |

框架现有 `TextDelta`、`ReasoningDelta` 和工具观测事件继续保留。插件消费带消息身份的
`OutputEvent`，避免同一段文本被发送两次。模型只返回完整结果时也能生成完整的消息事件。
流式文本和最终文本不一致时，插件结束该输出流并报告 `AGUI_TEXT_MISMATCH`。
RunResult 的最终回复与最近一条完整文本相同时不重复发送；否则补发完整文本消息。
因此，自定义 Loop 输出过中间说明后，返回的最终回复仍会进入文本事件流。

## 工具、并发与恢复

- 每次模型调用生成独立消息 ID，并为每个工具调用分配 `ToolCall.invocation_id`。
  供应商的 `ToolCall.id` 用于模型协议；`invocation_id` 用于执行、审批、检查点和 AG-UI。
  同一 Loop 或节点并行调用多个模型，即使供应商返回相同 ID，也不会混用结果。
- `EventEnvelope.node_execution` 保存节点执行身份。AG-UI `metadata.framework` 包含
  runId、executionId、sourceSequence 和节点信息。这里的 runId 是跨恢复不变的 Framework
  运行 ID；AG-UI 顶层 runId 是本次 executionId。
- 工具结果来自输出中间件处理后的结果。大内容已转存时，事件携带预览和引用；不会重新
  读取附件，将大数据塞回事件中。`TOOL_CALL_RESULT.content` 是包含上述字段的 JSON 字符串。
- ReAct 和 Workflow 在完整工具批次写入历史后发布结果；暂停时发布已完成的工具结果。
  一批工具中有慢调用时，其它工具的结果事件也可能延后。执行中状态仍可通过原有工具事件观察。
- Workflow 恢复时，工具结果沿用检查点中的 invocation_id，只发送原调用的结果，
  不重复声明工具或发送参数。应用应保留之前的会话消息，才能把结果显示在原调用下。
  执行器和插件重建后也不依赖内存中的旧映射。
- 审批和结构化输入沿用真实 interaction_id、提示和响应 schema。应用将用户答案转换成
  `InteractionResponse`，随新的 `ResumeRequest` 启动恢复。RecoveryRequired 仍需
  `CallResolution`，不能仅提交输入文本。
- 工具审批使用标准 `reason: "tool_call"`，`toolCallId` 指向对应的 invocation_id。
  审批通过或拒绝后，结果均关联这个调用；拒绝不会执行工具。
- 内置执行器将普通 `AwaitInput` 也保存为持久化字符串问答。应用通过
  `InteractionManager.answer()` 保存答案，再启动返回的恢复请求。只有未发布持久化交互的
  自定义 Loop 使用提示性 interrupt，此时不能将其 ID 作为可恢复的 interaction_id。
- 每次恢复使用新输出流及 executionId。该流不混入旧执行或子 Agent 的事件；子 Agent 可
  独立使用插件启动，本版本不自动合并为 SUBAGENT 事件。

## 自定义节点和客户端状态

自定义 Loop 或 WorkflowNode 可以使用通用输出事件，无须依赖 AG-UI 类型：

```rust
use eliteforge_agent_framework::OutputEvent;
use serde_json::json;

context.emit_output(OutputEvent::StateSnapshot(json!({"progress": 0})));
context.emit_output(OutputEvent::StateDelta(json!([
    {"op": "replace", "path": "/progress", "value": 50}
])));
context.emit_output(OutputEvent::Custom {
    name: "video.preview".into(),
    value: json!({"url": "https://example.com/preview.mp4"}),
});
```

这里的 `context` 是 `RunContext`；WorkflowNode 中使用 `context.run`。
自定义逐段输出要显式发送 MessageStarted、TextDelta 和 MessageFinished；工具结果使用
ToolResult，并保留模型返回的 `ToolCall.invocation_id`，不要为了发送结果重新创建 ToolCall。
完全自定义的工具调用使用 `ToolCall::new` 创建一次，将同一调用用于声明和结果。
状态快照是前端数据，不会自动暴露 ConversationStore、模型私有上下文或执行检查点。
辅助模型调用（例如摘要压缩）的文本和推理不会进入 UI 输出流。

## 容量与失败

`AgUiOptions` 默认最多缓存 256 条源事件，单条源事件或编码后的协议事件最多 1 MiB，
最多保留 1024 个尚未关闭的消息、工具、步骤和活动。可以按业务请求量调整。
活动快照在一次执行内保留配对记录，受同一个数量限制。
启动前会检查运行开始事件序列化后的大小，包括会话 ID 的 JSON 转义和重复字段。
超限会直接拒绝启动；合法的引号、反斜杠和 Unicode 会话 ID 可以正常使用。

`on_published` 按发布顺序执行，做有界检查与非阻塞入队。它不是生命周期 Hook，也不拦截执行；
普通 `on_event` 继续使用原有观测队列。慢观测 sink 不会挤掉 AG-UI 通道里的事件。
实现该同步方法时，不应进行 I/O、等待或再次发布事件。

通道满、消息超限、非法 Patch 或事件顺序错误，会关闭已经向客户端打开的消息/步骤，
随后发出一个明确的 RUN_ERROR。没有静默丢弃后继续宣告成功，也不会因此重复调用工具。
协议流失败和 Agent 执行失败是两件事；执行的最终结果仍以 `handle.wait()` 为准。
运行异常默认使用通用错误文本，宿主显式设置 `include_error_details` 后才输出异常正文。

此插件提供事件输出，不实现完整的 RunAgentInput HTTP 端点、客户端工具执行、
MESSAGES_SNAPSHOT 历史快照或持久化 SSE 重连回放。需要回放时，宿主保存已经转换好的
AG-UI 事件；现有 Workflow journal 不包含完整 token 流，不能据此还原丢失的文本增量。

## 验证

[protocol.rs](../crates/eliteforge-agent-official-plugin-agui/tests/protocol.rs) 使用真实 SDK 客户端消费输出，验证
文本与推理配对、工具参数/结果顺序、并发模型和节点隔离、附件引用、审批通过与拒绝、
执行器重建后的恢复、旧检查点兼容、最终回复、中间说明、转义会话 ID、状态与活动、
取消、重试、超限、错误文本和慢观测 sink。所有模型均为本地测试实现，不需要模型账号。
