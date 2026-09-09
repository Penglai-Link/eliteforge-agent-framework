# eliteforge-agent-framework

EliteForge Agent Framework 是一个可扩展的 Rust Agent 框架：Framework 负责装配、执行和生命周期管理，Plugin 提供模型、工具、存储和可观测能力，上层应用保留身份、权限和业务流程。

```mermaid
flowchart LR
    Host[上层应用] --> Builder[AgentBuilder]
    Builder --> Agent[Agent]
    Agent --> Loop[ReAct / Workflow / DAG]
    Loop --> Model[模型]
    Loop --> Tool[工具与 MCP]
    Loop --> Store[SQLite / 会话 / 状态]
    Agent --> OTel[OpenTelemetry]
```

## 快速上手

### 1. 准备环境

需要 Rust/Cargo、Rust 工具链和 C 编译工具。先执行环境检查，再拉取锁定依赖：

```sh
make doctor
make install
```

### 2. 连接真实模型

Framework 默认启用 Genai。真实模型验证放在与 `crates/` 平级的 [integration-tests](integration-tests/README.md) 项目中，配置模型名和凭据后显式运行：

```sh
export MODEL="your-model"
export OPENAI_API_KEY="your-api-key"
cargo run -p eliteforge-agent-integration-tests --features live-model --bin default_model
```

代理服务可额外设置 `MODEL_ENDPOINT` 和 `MODEL_API_KEY`。完整代码和其他模型协议见[快速上手](docs/quickstart.md)。

## 能力概览

- 执行：默认 ReAct、分阶段 Workflow、可并行和分支的 DAG。
- 扩展：模型、工具、MCP、用户问答、上下文、存储和 OpenTelemetry 均通过 Plugin 接入。
- 运行控制：流式事件、追加指令、取消、工具撤销和资源清理。
- 持久化：SQLite 会话、checkpoint、长期记忆、上下文摘要和中断恢复。
- 宿主边界：身份、权限、凭据、套餐和业务流程由上层应用管理。

## 关键 Crate

| Crate | 作用 |
| --- | --- |
| `eliteforge-agent-framework` | Agent 装配、循环调度、模型和工具执行 |
| `eliteforge-agent-core` | 公共类型、能力接口和 Plugin 契约 |
| `eliteforge-agent-plugin-kernel` | 服务注册、依赖排序和资源关闭 |
| `eliteforge-agent-official-plugin-*` | Genai、SQLite、MCP、AG-UI、OTel 等官方实现 |

## 文档

- [快速上手](docs/quickstart.md)：按场景运行 Agent，并查看完整 Rust 代码。
- [架构总览](docs/architecture.md)：分层、装配、执行链和生命周期。
- [Plugin 与扩展点](docs/plugins.md)：注册方式和现有实现。
- [DAG 工作流](docs/dag-workflows.md)：节点、并行、分支和恢复。
- [交互与审批](docs/interactions.md)：用户问答、工具审批和跨进程恢复。
- [可观测](docs/observability.md)：事件、OpenTelemetry 和恢复身份。

## 常用命令

```sh
make format  # 格式化
make lint    # 格式、Clippy、编译和 Rustdoc 检查
make test    # workspace 测试和文档测试
make build   # 构建 release 产物
```
