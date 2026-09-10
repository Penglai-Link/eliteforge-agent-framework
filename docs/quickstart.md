# 快速上手

本文按场景介绍如何组装和运行 Agent。先准备 Rust/Cargo 以及模型服务。

## 场景导航

| 场景 | 入口 |
|---|---|
| 最小可运行 Agent | [创建第一个 Agent](#创建第一个-agent) |
| 声明业务能力并按包自动装配 | [自动装配业务能力](#自动装配业务能力) |
| 一次配置官方插件 | [使用官方插件包](#使用官方插件包) |
| 边生成边推送给前端 | [流式输出](#流式输出) |
| 运行中追加指令或取消 | [控制运行](#控制运行) |
| 保持同一会话上下文 | [连续对话](#连续对话) |
| 固定步骤、并行节点或分支 | [使用 DAG](#使用-dag) |
| 一个连接下按阶段选择模型 | [按阶段选择模型](#按阶段选择模型) |
| 继续未完成的持久化运行 | [恢复运行](#恢复运行) |
| 增加固定系统指令 | [增加上下文](#增加上下文) |
| 接入 OpenTelemetry | [开启 OTel](#开启-otel) |

整体分层和运行语义见[Framework 架构](architecture.md)；扩展点和默认行为见[Plugin 与扩展点](plugins.md)。
下面的代码片段中，`agent` 是已构建的实例，`provider` 是按高级用法配置的 `Arc<GenaiChatAdapter>`，`?` 位于异步函数中；需要完整程序时，直接参考对应仓库示例源码。

## 创建第一个 Agent

这个场景使用一个 Genai 模型连接和 SQLite 存储。Framework 自动提供默认 ReAct、单模型选择、空工具集和基础上下文。

```rust
use eliteforge_agent_framework::{AgentBuilder, RunRequest};
use eliteforge_agent_official_plugin_storage_sqlite::SqliteStoragePlugin;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let agent = AgentBuilder::new()
        .plugin(SqliteStoragePlugin::new("agent.sqlite"))
        .build().await?;
    let result = agent.run(RunRequest::new("chat-1", "你好，请介绍一下自己")).await;
    let shutdown = agent.close().await;
    println!("{}", result?.output);
    if !shutdown.is_success() { return Err(format!("shutdown failed: {shutdown:?}").into()); }
    Ok(())
}
```

Framework 默认启用 Genai。真实模型冒烟程序位于独立的 [integration-tests](../integration-tests/README.md) 项目，配置模型名和凭据后显式启用 `live-model` 运行：

```sh
export MODEL="your-model"
export OPENAI_API_KEY="your-api-key"
cargo run -p eliteforge-agent-integration-tests --features live-model --bin default_model
```

代理服务可额外配置 `MODEL_ENDPOINT` 和 `MODEL_API_KEY`。其他协议通过 `MODEL_ADAPTER` 选择。需要在代码中指定时，直接 `.plugin(GenaiPlugin::new(model))`；详细配置见[模型插件](model-plugins.md)。

对应源码：[default_model.rs](../integration-tests/src/bin/default_model.rs)。`SqliteStoragePlugin` 会注册会话、执行状态、交互、长期记忆、摘要和附件存储；服务端通常复用同一个 Agent，进程退出时再调用 `agent.close()`。

## 自动装配业务能力

业务 crate 在对应 trait `impl` 上标记 `#[capability(...)]`，应用只选择它需要的能力包。
包配置属于当前 Agent；依赖已链接但未选中的包不会调用 factory。

~~~rust
use eliteforge_agent_core::CapabilityPackBuilder;
use eliteforge_agent_framework::AgentBuilder;
use eliteforge_agent_official_plugin_pack::EliteForgeOfficialPluginPack;
use shop_plugin::ShopConfig;

let agent = AgentBuilder::new()
    .plugin_pack(EliteForgeOfficialPluginPack::new("agent.sqlite"))
    .capability_pack(
        CapabilityPackBuilder::new(
            "shop",
            ShopConfig {
                name: "星河商店".into(),
            },
        ),
    )
    .build()
    .await?;
~~~

子包必须明确加入：

~~~rust
let shop = CapabilityPackBuilder::new("shop", shop_config)
    .include(CapabilityPackBuilder::new("shop.audit", audit_config));
~~~

这里的 `PluginPack` 只负责官方插件组合，`CapabilityPack` 负责业务声明的自动装配；
两者可以并用。完整的宏写法、`id`、`instance`、`depends_on`、配置 factory 和首版限制见
[能力包与自动装配](capability-packs.md)。

## 使用官方插件包

安装 `eliteforge-agent-official-plugin-pack` 后，一个包默认装配全部官方插件：模型、SQLite、用户问答、Skills、AGUI、MCP 和 OTel。Skills 默认按 Agent Skills 渐进式发现系统级目录与当前目录的本地技能。

```rust
use eliteforge_agent_framework::AgentBuilder;
use eliteforge_agent_official_plugin_pack::{
    EliteForgeOfficialPluginPack,
    agui::AgUiStream,
};

let agent = AgentBuilder::new()
    .plugin_pack(EliteForgeOfficialPluginPack::new("agent.sqlite"))
    .build().await?;
```

AGUI 已默认启用，调用 `agent.start_with_output::<AgUiStream>(request)` 取得运行和输出流。
消费流、等待结果和关闭 Agent 的完整程序见[离线插件包示例](../crates/eliteforge-agent-official-plugin-pack/examples/official_pack.rs)：

```sh
cargo run -p eliteforge-agent-official-plugin-pack --example official_pack -- agent.sqlite
```

这个示例使用本地演示模型，无需凭据。MCP 默认没有服务器，工具集为空；OTel 使用上层应用的全局 provider。
替换模型、配置 MCP 服务器和遥测 provider，以及兼容的自定义 `PluginPack`，见
[插件包](plugins.md#插件包)。

## 流式输出

`run(request).await` 等价于 `agent.wait(agent.start(request)?).await`。需要把中间事件推送给 UI 或 SSE 时，先启动运行，再取得事件接收器：

```rust
use eliteforge_agent_framework::{AgentEvent, RunRequest};
use tokio::sync::broadcast::error::RecvError;

let mut handle = agent.start(RunRequest::new("chat-1", "你好"))?;
let mut events = agent.events(&mut handle)?;

let consume = async {
    loop {
        match events.recv().await {
            Ok(envelope) => {
                if let AgentEvent::TextDelta(text) = envelope.event {
                    println!("收到文本片段：{text}");
                }
            }
            Err(RecvError::Lagged(count)) => {
                eprintln!("消费过慢，丢失 {count} 条事件");
            }
            Err(RecvError::Closed) => break,
        }
    }
};

let (result, ()) = tokio::join!(agent.wait(handle), consume);
println!("最终结果：{}", result?.output);
```

`agent.events(&mut handle)` 首次调用会保留运行启动后、取得接收器前产生的事件，后续调用只接收新事件。`agent.subscribe(&handle)` 只订阅调用之后的事件，不取走初始接收器。两者都是有界实时通道，收到 `Lagged` 时应用应提示输出不完整，并用最终结果校正展示。接入 AG-UI 时使用[AG-UI 示例](agui.md#注册与使用)。

## 控制运行

服务端可以把 `RunControl` 保存到 `run_id` 对应的内存表中，让接收用户操作的任务追加指令、取消运行或撤销尚未派发的工具：

```rust
let mut handle = agent.start(RunRequest::new("chat-1", "分析项目"))?;
let control = agent.control(&handle)?;
let running_agent = agent.clone();
let waiting = tokio::spawn(async move { running_agent.wait(handle).await });

agent.steer(&control, "重点分析插件注册流程")?;
// agent.cancel(&control)?;
// agent.revoke_tools(&control, &["dangerous-tool".to_string()])?;

let result = waiting.await??;
println!("最终结果：{}", result.output);
```

`steer()` 成功只表示消息进入内存队列，不会中断当前模型请求或工具调用。ReAct 和 Workflow 会在模型/工具交互之间消费它；DAG 需要明确目标节点。丢弃 `RunHandle` 或 `wait()` future 会取消该次运行，丢弃 `RunControl` 不会取消运行。下一次 Run 或恢复运行需要重新取得控制句柄。

## 连续对话

同一个 `session_id` 使用同一份会话历史：

```rust
let first = agent
    .run(RunRequest::new("chat-1", "我叫小明"))
    .await?;
let second = agent
    .run(RunRequest::new("chat-1", "我叫什么？"))
    .await?;

println!("{}\n{}", first.output, second.output);
```

同一 Agent 的同一 Session 同时只能有一个 Run。并发请求需要由上层应用排队，或使用不同的 session ID。

## 使用 DAG

DAG 适合固定步骤、并行节点、条件分支和汇合。下面的两个 ReAct 节点按顺序执行，后一个节点接收前一个节点的输出：

```rust
use eliteforge_agent_framework::{
    AgentBuilder, AgentLoopPlugin, DagEdge, DagNode, DagWorkflowDefinition,
    DagWorkflowLoop, Message, ModelDescriptor, RunRequest, WorkflowStage,
};

let mut outline = WorkflowStage::new("outline");
outline.instructions.push(Message::system("根据用户主题列出文章提纲。"));
let mut write = WorkflowStage::new("write");
write.instructions.push(Message::system("根据前一个节点的提纲写出完整文章。"));

let definition = DagWorkflowDefinition {
    id: "write-article".into(),
    version: 1,
    nodes: vec![DagNode::react(outline), DagNode::react(write)],
    edges: vec![DagEdge::new("outline", "write")],
    max_parallel_nodes: 1,
};

let agent = AgentBuilder::new()
    .plugin(eliteforge_agent_official_plugin_storage_sqlite::SqliteStoragePlugin::new("agent.sqlite"))
    .plugin(eliteforge_agent_official_plugin_model_genai::GenaiPlugin::from_adapter(provider, vec![ModelDescriptor::new(std::env::var("MODEL")?)]))
    .plugin(AgentLoopPlugin::new(DagWorkflowLoop::new(definition)?))
    .build()
    .await?;

let result = agent
    .run(RunRequest::new("article-1", "写一篇介绍 Rust 的短文"))
    .await?;
println!("{}", result.output);
agent.close().await;
```

完整的 SQLite DAG 示例见 [dag.rs](../crates/eliteforge-agent-state-sqlite/examples/dag.rs)。复杂 DAG 的节点类型、并行限制和恢复语义见[DAG 工作流](dag-workflows.md)。

## 按阶段选择模型

一个连接可以注册多个模型；上层应用只配置一次地址和认证，再用选择策略把不同阶段映射到模型：

```rust
use eliteforge_agent_framework::{FixedModelStrategy, ModelDescriptor, ModelRef, ModelStrategyPlugin};
use std::sync::Arc;

let planner = std::env::var("PLANNER_MODEL")?;
let writer = std::env::var("WRITER_MODEL")?;
let agent = AgentBuilder::new()
    .plugin(eliteforge_agent_official_plugin_storage_sqlite::SqliteStoragePlugin::new("agent.sqlite"))
    .plugin(eliteforge_agent_official_plugin_model_genai::GenaiPlugin::from_adapter(provider, vec![
        ModelDescriptor::new(planner),
        ModelDescriptor::new(writer.clone()),
    ]))
    .plugin(ModelStrategyPlugin(Arc::new(FixedModelStrategy(
        ModelRef::new("default", writer),
    ))))
    .build()
    .await?;
```

上例固定使用 `writer`。阶段映射和故障回退见 [shared_connection.rs](../crates/eliteforge-agent-official-plugin-model-genai/examples/shared_connection.rs)，其中 `StageModelStrategy` 为 `plan` 和 `execute` 选择不同模型。

## 增加上下文

只想给默认 Agent 增加一条系统指令时，注册 `ContextPlugin`：

```rust
use eliteforge_agent_framework::{ContextPlugin, FixedContext};

let agent = AgentBuilder::new()
    .plugin(eliteforge_agent_official_plugin_storage_sqlite::SqliteStoragePlugin::new("agent.sqlite"))
    .plugin(eliteforge_agent_official_plugin_model_genai::GenaiPlugin::from_adapter(provider, vec![ModelDescriptor::new(std::env::var("MODEL")?)]))
    .plugin(ContextPlugin::new(
        "prompt",
        FixedContext::new("用简短的中文回答。"),
    ))
    .build()
    .await?;
```

上下文块预算、排列顺序、动态贡献者、摘要和工具结果引用见[上下文窗口](context-windows.md)。

## 恢复运行

持久化运行通过 `run_id` 恢复。先从 `start()` 取得 ID，运行中断后重新构造同一类 Agent，再带上 `ResumeRequest`：

```rust
let handle = agent.start(RunRequest::new("article-2", "介绍 Rust"))?;
let run_id = handle.run_id;
let result = agent.wait(handle).await?;
println!("{:?}: {}", result.status, result.output);

let mut request = RunRequest::new("article-2", "");
request.resume = Some(ResumeRequest {
    run_id,
    ..Default::default()
});
let result = agent.run(request).await?;
```

已完成的运行直接返回保存的结果。等待审批的运行还要附带交互回答，不能只传 `run_id`。可跨进程运行确定性审批示例：

```sh
cargo run -p eliteforge-agent-state-sqlite --example durable_approval -- start /tmp/approval.db 17
# 使用上一步返回的 run_id 和 interaction.id
cargo run -p eliteforge-agent-state-sqlite --example durable_approval -- answer /tmp/approval.db <run-id> <interaction-id> true
```

对应源码：[durable_approval.rs](../crates/eliteforge-agent-state-sqlite/examples/durable_approval.rs)。真实应用需要由上层应用负责鉴权、审批 UI 和回答提交。

## 开启 OTel

创建 `TelemetryPlugin` 并在 Agent 中注册。下面使用内存 exporter 验证 span 名称：

```rust
use eliteforge_agent_official_plugin_otel::{TelemetryConfig, TelemetryPlugin};
use opentelemetry_sdk::{
    metrics::SdkMeterProvider,
    trace::{InMemorySpanExporter, SdkTracerProvider},
};

let exporter = InMemorySpanExporter::default();
let traces = SdkTracerProvider::builder()
    .with_simple_exporter(exporter.clone())
    .build();
let metrics = SdkMeterProvider::default();
let telemetry = TelemetryPlugin::new(
    "telemetry",
    &traces,
    &metrics,
    TelemetryConfig::default(),
)?;

let agent = AgentBuilder::new()
    .plugin(eliteforge_agent_official_plugin_storage_sqlite::SqliteStoragePlugin::new("agent.sqlite"))
    .plugin(telemetry)
    .plugin(eliteforge_agent_official_plugin_model_genai::GenaiPlugin::from_adapter(provider, vec![ModelDescriptor::new(std::env::var("MODEL")?)]))
    .build()
    .await?;

let result = agent.run(RunRequest::new("trace-1", "你好")).await;
agent.close().await;
traces.force_flush()?;
for span in exporter.get_finished_spans()? {
    println!("{}", span.name);
}
traces.shutdown()?;
metrics.shutdown()?;
println!("{}", result?.output);
```

默认 ReAct 会看到 `agent.run` 和 `agent.model`；使用 DAG 或工具时会增加对应的 node/tool span。生产环境由上层应用配置 exporter、采样和关闭策略，详见[可观测](observability.md)。

## 下一步

- 了解模块边界、执行链和恢复语义：阅读[Framework 架构](architecture.md)。
- 接入自定义模型、工具、上下文或中间件：阅读[插件开发指南](plugins.md#按场景选择扩展点)。
- 接入审批、AG-UI、MCP 和 SQLite：阅读[用户交互](interactions.md)、[AG-UI](agui.md)、[SQLite 存储](sqlite-storage.md)。
- 运行仓库验证：参见[实现状态](implementation-status.md)。
