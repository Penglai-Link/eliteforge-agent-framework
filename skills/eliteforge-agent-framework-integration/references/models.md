# 模型接入

入门程序通过官方包的 `model` 模块导出 Genai 类型。若不使用官方包，依赖 `eliteforge-agent-official-plugin-model-genai`，Git URL/tag 与 Framework 相同，将下方导入前缀改为该 crate 即可。

## 一个端点，一个模型

以下函数接收宿主配置，不读取或打印凭据：

```rust
use eliteforge_agent_official_capability_pack::model::{AdapterKind, AuthData, GenaiPlugin};

fn chat_plugin(model: &str, endpoint: &str, auth: AuthData) -> GenaiPlugin {
    GenaiPlugin::new(model)
        .connection("primary")
        .adapter(AdapterKind::OpenAI)
        .endpoint(endpoint)
        .auth(auth)
}
```

将返回值传给官方包的 `.model(...)` 或 builder 的 `.plugin(...)`。入门程序的 `AuthData::from_env("ELITEFORGE_SKILL_AGENT_FRAMEWORK_API_KEY")` 可直接作为 `auth`。不重复安装官方包原有模型。

| 服务协议 | `AdapterKind` | 注意 |
| --- | --- | --- |
| OpenAI Chat Completions 兼容 | `OpenAI` | 服务根地址通常含 `/v1/` |
| OpenAI Responses | `OpenAIResp` | 不要以为名称相近就能与 Chat Completions 互换 |
| Anthropic 原生协议 | `Anthropic` | 使用该协议对应端点和凭据 |
| Gemini 原生协议 | `Gemini` | 使用对应 API 版本前缀 |
| Ollama | `Ollama` | 本地无认证服务可用 `AuthData::None` |
| Cohere | `Cohere` | 核对模型实际支持的操作 |

`GenaiPlugin::new(model)` 使用显式配置；`default()` 才读取框架默认环境配置。模板选显式配置以避免隐含模型来源。使用不同协议时，同时更新 adapter、endpoint、认证和模型能力，不仅修改模型名。

## 同一连接注册多个模型

`connection` 是宿主定义的连接身份，`model` 是该连接内的模型 ID。示例函数中的三个参数均由宿主配置提供：

```rust
use eliteforge_agent_framework::ModelDescriptor;
use eliteforge_agent_official_capability_pack::model::{AdapterKind, AuthData, GenaiPlugin};

fn multi_model_plugin(endpoint: &str, planner: &str, writer: &str, auth: AuthData) -> GenaiPlugin {
    GenaiPlugin::new(planner)
        .chat_model(ModelDescriptor::new(writer))
        .connection("primary")
        .adapter(AdapterKind::OpenAI)
        .endpoint(endpoint)
        .auth(auth)
}
```

模型 ID 不能重复；如果 planner 与 writer 相同，只注册一次。Agent 固定选择其中一个：

```rust
use eliteforge_agent_framework::{AgentSelection, ModelRef};
let selection = AgentSelection {
    model: Some(ModelRef::new("primary", writer)),
    ..Default::default()
};
```

`writer` 是上面已注册的真实模型 ID。连接 ID 是 `primary`，插件 ID 为 `model.genai.primary`，不能把后者写进 `ModelRef`。

多个端点分别创建不同 `.connection(...)` 的插件。需要阶段策略时，在 builder 上注册：

```rust
use eliteforge_agent_framework::*;
use std::sync::Arc;
let builder = builder.plugin(ModelStrategyPlugin(Arc::new(FixedModelStrategy(
    ModelRef::new("primary", writer),
))));
let selection = AgentSelection {
    model_strategy: Some("model.strategy".into()),
    ..Default::default()
};
```

阶段映射用 `StageModelStrategy { stages, fallback }`，值为 `Arc<dyn ModelSelectionStrategy>`。阶段名称必须与实际执行器传给模型的阶段一致；策略本身不会创建“规划/执行”工作流。显式 `model` 与策略同时存在时先明确调用优先级，不通过添加更多插件猜测选择结果。

## 自定义模型适配器

已有模型 SDK 时，实现 Core 的 `ChatModel`，保留流和最终结果的一致性。下面是完整的离线演示实现，只用于理解协议，不是生产模型：

```rust
use async_trait::async_trait;
use eliteforge_agent_framework::*;

struct DemoModel;
#[async_trait]
impl ChatModel for DemoModel {
    async fn generate(
        &self, _: &str, _: ChatRequest, stream: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        let answer = "这是本地适配器的固定答复。";
        stream.text_delta(answer).await;
        Ok(ChatResponse::text(answer))
    }
}
```

通过 `.model(ChatModelPlugin::new(DemoModel))` 替换官方包模型。生产适配器将 `ChatRequest` 转为 SDK 输入，转发文本增量、工具调用、用量和结束原因；不要丢弃思考续接数据、工具结果配对或多模态块。不支持的输入应返回明确错误。

## Embedding 与其他模型操作

在已有 Genai 插件链上加 `.embedding_model(embedding_model_id)`；该 ID 必须是服务支持的向量模型。Chat 和 Embedding 可共用认证，但不是同一个操作绑定。

注册完成并构建 Agent 后：

```rust
use eliteforge_agent_framework::{EmbeddingRequest, ModelRef};
let mut request = EmbeddingRequest::texts(["订单退款期限", "商品配送方式"]);
request.choice.model = Some(ModelRef::new("primary", embedding_model_id));
let response = agent.embed(request).await?;
// 将 response 中的向量及其模型/空间信息一并交给宿主索引层。
```

这直接执行向量操作，不让 Chat 模型生成工具调用。检索时必须沿用建库的 `EmbeddingSpace`，调用 `agent.embed_query(&space, request)`；不能用自动路由随机更换向量空间。模型、版本、维度和预处理变化需要新索引或显式迁移。

| 操作 | Agent 入口 | 所需实现 |
| --- | --- | --- |
| 文本/多模态向量 | `embed` / `embed_query` | `EmbeddingModel` |
| 文档重排 | `rerank` | `RerankModel` |
| 图像生成 | `generate_image` | `ImageGenerationModel` |
| 视频任务 | `generate_video` | `VideoGenerationModel` |
| 语音识别/合成 | 对应语音操作 | `SpeechRecognitionModel` / `SpeechSynthesisModel` |
| 实时会话 | 对应 realtime 操作 | `RealtimeModel` |

Core 有契约不等于 Genai 实现了所有操作。需要对应提供方插件或 PersonAI 包注册实现；不要把所有模型操作都强行转成 Chat HTTP 请求。

只生成标题/摘要、不需要 Agent 会话时，用宿主已构造的 `ModelClient`。固定模型走 `chat`；自动文本选择走 `text_generation(request, &limits, stage)`；需要用量报告走对应 `*_observed` 调用。它们不自动创建用户会话或持久化对话。

## 故障处理

端点 404 先核对基础路径与协议，401/403 先核对当前账号和授权。不能通过无条件换模型规避认证错误。`ModelRecoveryPlugin(ModelRecoveryPolicy { candidates, allow_explicit_model_replacement: false })` 可为符合条件的模型不可用错误配置候选恢复；默认不替换用户显式选定模型，已输出内容或预算不足时也不会随意重试。

不要把 tokenizer 估算当账单用量。上下文预算遵守模型窗口，费用统计优先使用供应商返回的实际 usage。
