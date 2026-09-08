# 用户问答与工具审批

`ReactAgentLoop`、`WorkflowLoop` 和 DAG 内置 ReAct 节点都支持持久化问答及工具审批。
等待用户期间没有后台任务或模型请求占用资源，本次 AG-UI 流以 `RUN_FINISHED` 的
`interrupt` 结果结束。收到答案后，应用发起下一次执行。

## 默认插件

配置模型后，加入 SQLite 存储和问答工具：

```rust
use agent_framework::*;
use agent_storage_sqlite::SqliteStoragePlugin;
use agent_tools_human::HumanInteractionPlugin;
use agent_agui::AgUiPlugin;

let agui = AgUiPlugin::default();
let agent = AgentBuilder::new()
    .plugin(SqliteStoragePlugin::new("agent.sqlite"))
    .plugin(provider.plugin(models)) // 已配置的模型连接和目录
    .plugin(HumanInteractionPlugin)
    .plugin(agui.clone())
    .build()
    .await?;

let (handle, mut stream) = agui.start(&agent, RunRequest::new("chat-1", "帮我剪辑视频"))?;
let send = async {
    while let Some(frame) = stream.recv_sse().await? {
        // 应用将 frame 写入 HTTP SSE 响应。
        println!("{frame}");
    }
    Ok::<_, Box<dyn std::error::Error>>(())
};
let (result, sent) = tokio::join!(handle.wait(), send);
sent?;
let result = result?;
// result.status == AwaitingInput 时，result.interactions 包含问题。
```

`SqliteStoragePlugin` 注册以下服务，数据保存在同一个文件的不同表中。
Framework 只依赖存储抽象。数据库路径由应用指定；缺少存储不会回退到内存。

| 扩展点 | 注册类型 | 默认实现 |
|---|---|---|
| `ConversationStore` | `ConversationStoreService` | `SqliteConversationStore` |
| `StateStore` | `StateStoreService` | `SqliteStateStore` |
| `InteractionStore` | `InteractionStoreService` | `SqliteStateStore` 的独立交互表 |
| `LongTermMemory` | `LongTermMemoryService` | `SqliteLongTermMemory` |
| `SummaryStore` | `SummaryStoreService`，位于存储组合插件 | `SqliteSummaryStore` |
| `ArtifactStore` | `ArtifactStoreService`，位于存储组合插件 | `SqliteArtifactStore` |

存储组合插件拥有连接，在 `agent.close().await` 时关闭。长期记忆检索、模型摘要、工具结果
转存仍需显式接入对应组件，注册存储不会自动调用模型或写入长期记忆。
应用也可分别注册存储，以共享连接或替换实现。`FileArtifactStore` 仍可显式选择；
默认组合插件使用 SQLite 保存附件字节，不提供内存存储实现。
跨平台构建与可选分词、向量扩展见[SQLite 存储](sqlite-storage.md)。

## ask_user

`HumanInteractionPlugin` 注册 `AskUserTool`，模型看到的名称为 `ask_user`。
一次包含 1–3 个问题，每题 2–5 个选项，默认允许自由输入和跳过。

模型参数示例：

```json
{
  "intro": "先确定视频格式",
  "questions": [{
    "question": "使用哪种画幅？",
    "options": [
      {"label": "横屏", "description": "16:9"},
      {"label": "竖屏", "description": "9:16"}
    ],
    "allow_free_text": true,
    "allow_skip": true
  }]
}
```

工具为问题和选项生成 UUID。`InteractionRequest.presentation` 包含
`kind: "ask_user"`、`intro` 和带 ID 的 `questions`，前端据此渲染。
`response_schema` 定义实际回答格式；每题必须提交且只能提交以下一种答案：

```json
{
  "answers": {
    "<question-id>": {"option_id": "<option-id>"}
  }
}
```

自由输入使用 `{"text":"自定义要求"}`，跳过使用 `{"skip":true}`。
未开启的回答方式、其他题目的选项、未知问题、漏答、重复字段组合和空白文本都会被拒绝。
介绍及问题最多 500 字符，选项标题 120 字符、说明 240 字符，自由答案 4000 字符。

问题及工具结果先保存，然后结束本次执行。相同批次中尚未执行的工具记为未执行；
答案以新的用户消息追加到历史，模型据此重新决定调用及参数。已经写入的原始问题不会被答案覆盖。

## 查询和回答

页面刷新或服务重启后，可查询待回答的问题，不需要内存中的 `RunHandle`。

```rust
let store = agent.service::<InteractionStoreService>()
    .expect("SqliteStoragePlugin registered InteractionStore");
let interactions = InteractionManager(store.0.clone());
let pending = interactions.pending("chat-1").await?;

// session_id 必须由应用完成身份验证后确定。
let request = interactions.answer("chat-1", interaction_id, answer_json).await?;
let (handle, stream) = agui.start(&agent, request)?;
// 将新的 stream 写入下一次 HTTP SSE 响应，并等待 handle.wait()。
```

保存答案和启动执行是两个步骤。保存后进程退出，可以重新调用 `answer()` 提交相同答案；
也可以使用对应 `ResumeRequest { run_id, response: None, resolutions: vec![] }` 恢复，
执行器会读取已保存的答案。相同答案可以重试，不同答案产生冲突；过期问题不能接受新答案。
已按时保存的答案在到期后仍可用于恢复。

`InteractionRecord` 保存会话、逻辑运行 ID、DAG 节点调用 ID、原始工具调用、问题、答案和时间。
AG-UI 每次恢复使用新的 `runId`；Framework 的逻辑 `run_id` 保持不变，用于找到原执行记录。
每次执行都有独立的 `execution_id` 和控制句柄。当前 `ResumeRequest` 可以携带一个答案；
多个待回答问题可先分别保存答案，再恢复执行。

## 工具审批

`ToolPolicy` 决定允许、拒绝或要求审批。默认策略列表为空，应用按工具性质添加规则。

```rust
use async_trait::async_trait;

struct ConfirmExport;
#[async_trait]
impl ToolPolicy for ConfirmExport {
    async fn evaluate(
        &self, invocation: &ToolInvocationContext<'_>,
    ) -> Result<ToolPolicyDecision, AgentError> {
        Ok(if invocation.descriptor.id == "video.export" {
            ToolPolicyDecision::RequireApproval("确认导出视频？".into())
        } else {
            ToolPolicyDecision::Allow
        })
    }
}

// 在前面的 AgentBuilder 链中加入：
// .plugin(ToolPolicyPlugin::new("video.approval", ConfirmExport))
```

审批之前会保存工具调用 ID、工具定义及原始参数，并检查整个批次的策略。
审批回复必须是 JSON 布尔值：

```rust
let approved = interactions.answer("chat-1", interaction_id, serde_json::json!(true)).await?;
let result = agent.run(approved).await?;
```

同意后执行原始调用；拒绝后写入工具错误结果。恢复时仍检查工具是否可用、参数定义是否变化及
当前策略。不会让模型把“同意”解释成另一次任意调用。

```text
模型提出调用 → 检查整个批次 → 保存待审批调用 → 结束本次执行
                                                ↓ 用户决定
                      同意 → 标记批准 → 标记开始 → 调用工具 → 保存结果
                      拒绝 → 保存拒绝结果
                                                ↓
                                           模型继续执行
```

工具已开始、但结果因崩溃或断线无法确认时，返回 `RecoveryRequired`。
应用核实后使用 `CallResolution::Record` 记录实际结果，或明确选择 `Retry`；
保存过的审批不等于允许框架自动重做未知结果的外部操作。

## 执行代码与持久化

ReAct 与 Workflow 通过公共 `ModelExecutionLoop` 执行模型及工具，DAG 默认节点使用同一部分。
`InteractionStore` 独立提供查询和答案保存；问答格式属于工具插件，AG-UI 只负责协议映射。
HTTP、身份验证、卡片 UI 和业务状态仍由应用实现。

执行检查点先保存，再幂等发布交互记录，最后发送可见的中断结果。
如果在两次写入间退出，恢复会根据检查点补齐交互记录；未完成补写前不会执行待审批工具。
ReAct 的 ConversationStore 同样在检查点提交后按前缀追加，恢复可补齐尚未同步的历史。
这些步骤不假设不同存储抽象之间共享数据库事务。

回归测试：[三种执行器的问答与 ReAct 审批](../crates/agent-storage-sqlite/tests/interactions.rs)、
[SQLite 存储重开与答案竞争](../crates/agent-storage-sqlite/tests/stores.rs)。
