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

插件注册输出工厂和事件服务。`agent.start()` 在执行前创建本次运行的输出通道，
然后发布事件；应用不需要保留插件实例。

```rust
use eliteforge_agent_official_plugin_agui::{AgUiPlugin, AgUiStream};
use eliteforge_agent_framework::RunRequest;

let agent = builder.plugin(AgUiPlugin::default()).build().await?;

let mut handle = agent.start(RunRequest::new("chat-1", "分析项目"))?;
let mut events = agent.output::<AgUiStream>(&mut handle)?;
let control = agent.control(&handle)?;

let consume = async {
    while let Some(frame) = events.recv_sse().await? {
        // HTTP handler 将 frame 写入 text/event-stream 响应体。
        print!("{frame}");
    }
    Ok::<(), eliteforge_agent_official_plugin_agui::ag_ui::Error>(())
};
let (result, delivered) = tokio::join!(agent.wait(handle), consume);
delivered?;
let result = result?;
```

`agent.output()` 只能取出一次对应类型的输出。需要在启动前确认插件已安装时，可用
`agent.start_with_output::<AgUiStream>(request)` 同时取得句柄和输出流；缺少插件会在执行前报错。

`events.recv()` 返回 SDK 的 `Event`，`events.recv_sse()` 返回编码后的 SSE 帧。
消费方应识别 `RUN_ERROR`；`recv_sse()` 的 `Err` 只表示编码失败。
SSE 内容直接使用 AG-UI JSON，不再套一层应用的普通 JSON 响应体：

```text
data: {"type":"TEXT_MESSAGE_CONTENT","messageId":"...","delta":"你好"}

```

`control` 可以交给接收用户消息的任务，继续调用 `agent.steer(&control, ...)`、`agent.cancel(&control)` 或 `agent.revoke_tools(&control, ...)`。
保留控制句柄不会阻止事件流结束。丢弃事件接收器只停止消费；丢弃 `RunHandle` 或它的
等待 future 会取消执行。HTTP 断开时是否取消 Agent，由宿主决定。

可直接运行不需要模型账号的完整示例：

```sh
cargo run -p eliteforge-agent-official-plugin-agui --example sse
```

源码：[sse.rs](../crates/eliteforge-agent-official-plugin-agui/examples/sse.rs)。服务端复用 Agent，退出时调用 `agent.close()`。

## 事件映射

框架内部事件记录 Agent 执行过程中发生的事情，插件将其转换成前端接收的 AGUI 事件。
下表省略 `AgentEvent::` 和 `OutputEvent::` 前缀；`OutputEvent` 通过 `AgentEvent::Output` 发布。
「数据」列说明转换后的 AGUI 关键字段，不是完整 JSON。

| 框架内部事件 | AGUI事件 | 描述 | 数据 |
|---|---|---|---|
| `RunStarted` | `RUN_STARTED` | 开始本次运行 | `threadId`：会话 `session_id`；`runId`：本次 `executionId`，恢复时生成新值 |
| `StepStarted / StepFinished` | `STEP_STARTED / STEP_FINISHED` | 开始或结束一个步骤，例如调用模型；结束不代表成功 | `stepName`：步骤名称，附带节点执行身份和步骤 ID，用于区分同名步骤 |
| `WorkflowNodeChanged` | `STEP_STARTED / STEP_FINISHED`；`CUSTOM` | Workflow 节点开始运行时打开步骤，离开运行状态时结束步骤，并发送节点状态 | `stepName`：`node/{node_id}`；自定义事件 `name="framework.node"`，`value` 包含 `nodeId` 和 `status` |
| `MessageStarted / TextDelta / MessageFinished` | `TEXT_MESSAGE_START / TEXT_MESSAGE_CONTENT / TEXT_MESSAGE_END` | 开始消息、逐段输出文字、结束消息；首段非空文本到达时打开消息，最终回复补齐未流式返回的后缀 | `messageId`：消息 ID；开始事件的 `role="assistant"`；内容事件的 `delta`：本次追加的文字，例如 `"你好"` |
| `ReasoningDelta`；消息结束或中止事件 | `REASONING_START / REASONING_MESSAGE_START / REASONING_MESSAGE_CONTENT / REASONING_MESSAGE_END / REASONING_END` | 逐段发送模型公开返回的推理文本，在消息结束或中止时关闭；可配置关闭推理输出 | 推理 ID 由原消息 ID 加 `/reasoning` 生成；内容事件的 `delta`：本次追加的推理文本 |
| `MessageFinished.tool_calls` | `TOOL_CALL_START / TOOL_CALL_ARGS / TOOL_CALL_END` | 声明模型要调用的工具并发送参数；END 表示参数发送结束，工具结果随后到达 | `toolCallId`：调用的 `invocation_id`；`toolCallName`：工具名称；`parentMessageId`：所属消息；ARGS 的 `delta`：完整参数 JSON 字符串 |
| `ToolResult` | `TOOL_CALL_RESULT` | 返回工具执行结果，并关联原工具调用 | `toolCallId`：原调用 ID；`messageId`：结果消息 ID；`content`：JSON 字符串，包含 `content`、`structured`、`isError`、`effect`、`resultReference` |
| `StateSnapshot` | `STATE_SNAPSHOT` | 应用主动发送一份完整的前端状态 | `snapshot`：完整状态，例如 `{"progress":0}` |
| `StateDelta` | `STATE_DELTA` | 应用主动更新前端状态的部分字段 | `delta`：RFC 6902 补丁，例如 `[{"op":"replace","path":"/progress","value":50}]` |
| `ActivitySnapshot` | `ACTIVITY_SNAPSHOT` | 应用发送一项活动的完整内容，例如任务进度卡片 | `messageId`：插件生成的活动消息 ID；`activityType`：活动类型；`content`：完整活动内容 |
| `ActivityDelta` | `ACTIVITY_DELTA` | 更新已发送的活动内容 | `messageId`、`activityType`：与活动快照配对；`patch`：RFC 6902 补丁 |
| `Custom` | `CUSTOM` | 应用发送自行约定的通知，例如视频预览地址 | `name`：事件名称；`value`：事件数据，例如 `name="video.preview"`、`value={"url":"…"}` |
| `Steered` | `CUSTOM` | 运行中追加的用户指令已写入历史，模型未必已经处理 | `name="framework.steered"`；`value.text`：追加的指令文本 |
| `MessageFinished`（含非文本内容或结构化回复） | `CUSTOM` | 传递图片等非文本内容或结构化数据，保留原始内容类型 | `name="framework.message"`；`value` 包含 `messageId`、`content`、`structured` |
| `RunFinished { status: Completed }` | `RUN_FINISHED` | 本次运行正常完成 | `threadId`、`runId`：会话与本次执行 ID；`outcome` 表示 `success`；已收到 `OutputEvent::RunResult` 时，`result` 携带最终 `RunResult` |
| `RunFinished { status: AwaitingInput / RecoveryRequired }` | `RUN_FINISHED` | 本次执行暂停，等待用户回答或人工核对执行结果 | `outcome` 表示 `interrupt`；中断信息包含 ID 和原因；有持久化交互时附带提示、响应 schema，工具审批还携带 `toolCallId` |
| `RunFinished { status: StepLimit / ToolCallLimit }` | `RUN_ERROR` | 达到最大执行步数或工具调用次数 | `code="AGENT_LIMIT_REACHED"`；`message`：执行次数超限说明 |
| `RunCancelled` | `RUN_ERROR` | 本次运行被取消 | `code="AGENT_CANCELLED"`；`message`：取消说明 |
| `RunFailed` | `RUN_ERROR` | 本次运行执行失败 | `code="AGENT_RUN_FAILED"`；`message`：默认通用错误说明，开启 `include_error_details` 后返回异常正文 |

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
  `agent.answer()` 保存答案，再启动返回的恢复请求。只有未发布持久化交互的
  自定义 Loop 使用提示性 interrupt，此时不能将其 ID 作为可恢复的 interaction_id。
- 每次恢复使用新输出流及 executionId。该流不混入旧执行或子 Agent 的事件；子 Agent 可
  配置插件后通过自己的 `agent.start()` 启动，本版本不自动合并为 SUBAGENT 事件。

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
协议流失败和 Agent 执行失败是两件事；执行的最终结果仍以 `agent.wait(handle)` 为准。
运行异常默认使用通用错误文本，宿主显式设置 `include_error_details` 后才输出异常正文。

此插件提供事件输出，不实现完整的 RunAgentInput HTTP 端点、客户端工具执行、
MESSAGES_SNAPSHOT 历史快照或持久化 SSE 重连回放。需要回放时，宿主保存已经转换好的
AG-UI 事件；现有 Workflow journal 不包含完整 token 流，不能据此还原丢失的文本增量。

## 验证

[protocol.rs](../crates/eliteforge-agent-official-plugin-agui/tests/protocol.rs) 使用真实 SDK 客户端消费输出，验证
文本与推理配对、工具参数/结果顺序、并发模型和节点隔离、附件引用、审批通过与拒绝、
执行器重建后的恢复、旧检查点兼容、最终回复、中间说明、转义会话 ID、状态与活动、
取消、重试、超限、错误文本和慢观测 sink。所有模型均为本地测试实现，不需要模型账号。
