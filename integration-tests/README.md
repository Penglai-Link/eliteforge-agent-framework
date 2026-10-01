# 真实服务集成验证

本项目与 `crates/` 平级，是不发布的 workspace 成员，通过公开 API 组装 Framework 和官方插件。
`default_model` 是手动运行的真实模型冒烟程序，打印模型响应并检查 Agent 关闭结果。

在仓库根目录配置模型和凭据后运行：

```sh
export MODEL="your-model"
export OPENAI_API_KEY="your-api-key"
cargo run -p eliteforge-agent-integration-tests --features live-model --bin default_model
```

代理服务可设置 `MODEL_ENDPOINT` 和 `MODEL_API_KEY`；其他协议通过 `MODEL_ADAPTER` 选择，见[模型配置](../docs/模型插件设计.md)。
运行会访问真实模型，并在当前工作目录创建或复用 `agent.sqlite`，同一 `chat-1` 会话会保留历史。

`live-model` 默认关闭，普通 `make test` 跳过此程序。启用全部 feature 的测试使用 Rust 测试入口，也不会执行调用模型的 `main`；只有显式 `cargo run` 才调用真实模型。
仅检查编译可以执行：

```sh
cargo check -p eliteforge-agent-integration-tests --features live-model --bin default_model
```

完整源码见 [default_model.rs](src/bin/default_model.rs)，其他 Agent 用法见[快速上手](../docs/快速上手.md)。
