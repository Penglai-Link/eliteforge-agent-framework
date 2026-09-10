# 能力包与统一装配

能力包是应用选择业务能力的入口。一个能力包可以同时带上接口实现、普通插件、工具、模型绑定和
Skills 资源。业务开发通常只需要做三件事：**声明接口、实现接口、让应用选择能力包**。

`CapabilityPack` 和 `PluginPack` 可以并存。前者负责按包 ID 发现和创建声明式成员；后者仍适合
把已经创建好的插件组合起来。两者最终进入同一个插件依赖图和同一套生命周期。

## 最小扩展点

插件 crate 在 trait 上声明扩展点，能力包只实现 trait。业务代码不需要再写 `Service` 包装类型、
`registration` 或实现所绑定的插件类型。

~~~rust
use eliteforge_agent_core::{capability, extension_point};

#[extension_point]
pub trait PaymentRule: Send + Sync {
    fn allow(&self, order_id: &str) -> bool;
}

#[derive(Default)]
pub struct ShopPaymentRule;

#[capability(pack = "shop")]
impl PaymentRule for ShopPaymentRule {
    fn allow(&self, _: &str) -> bool {
        true
    }
}
~~~

默认扩展点允许多个实现。消费者在 `Plugin::ready` 中读取完整集合：

~~~rust
use eliteforge_agent_core::{Plugin, PluginError, PluginReadyContext};

struct PaymentPlugin;

#[async_trait::async_trait]
impl Plugin for PaymentPlugin {
    fn id(&self) -> &str { "payment.consumer" }

    async fn install(
        &self,
        _: &mut eliteforge_agent_core::PluginContext<'_>,
    ) -> Result<(), PluginError> {
        Ok(())
    }

    async fn ready(&self, context: &mut PluginReadyContext<'_>) -> Result<(), PluginError> {
        let rules = context.extensions::<dyn PaymentRule>();
        // 绑定或校验所有规则；顺序是插件注册顺序。
        Ok(())
    }
}
~~~

全局唯一的角色写成 `#[extension_point(single)]`，消费者使用
`context.extension::<dyn Trait>()`。同一个 Agent 内出现第二个实现会在安装阶段报出两个插件 ID，
不会静默覆盖。

框架内置接口也进入同一扩展注册表，同时保留原有 `ContextService`、`ToolProviderService` 等服务
类型和注册适配器，已有应用不需要迁移。普通插件、官方插件和 `#[capability]` 声明的实现因此可以
被同一套 `extensions::<dyn Trait>()` 查询到。

## 选择包与构造实例

实现声明只是静态描述。只有应用选择对应包，工厂才会为当前 Agent 创建实例：

~~~rust
use eliteforge_agent_core::CapabilityPackBuilder;
use eliteforge_agent_framework::AgentBuilder;

let agent = AgentBuilder::new()
    .capability_pack(
        CapabilityPackBuilder::new("shop", ShopConfig::production())
            .include(CapabilityPackBuilder::new("shop.audit", AuditConfig::default())),
    )
    .build()
    .await?;
~~~

`include(...)` 是唯一会启用子包的途径；链接某个 crate 或在 `depends_on` 中提到另一个能力，
都不会隐式启用它。不同 Agent 各自构造、关闭自己的实例。未选包既不运行 factory，也不发现其
Skills 或准备其资源。

需要包配置时，保留已有的 trait-impl 写法：

~~~rust
#[capability(
    pack = "shop",
    id = "shop.context",
    config = ShopConfig,
    factory = ShopContext::from_config
)]
#[async_trait::async_trait]
impl ContextProvider for ShopContext {
    // ...
}
~~~

`config` 与 `factory` 必须成对出现，factory 是同步的 `fn(&Config) -> Self`。省略它们时，
实现类型使用 `Default::default()`。`id`、`instance` 和 `depends_on` 都是可选的；一个类型实现
多个接口时默认共享同一个实例，需独立实例时显式指定不同的 `instance`。

## 工厂声明：有配置的工具、模型和普通插件

有些角色不能从 trait 实现推断出完整绑定信息。例如单个工具需要描述、模型需要连接与模型 ID，
布局和路由本身是配置值。此时用 `#[capability_factory]` 返回一个具体 `Plugin`：

~~~rust
use eliteforge_agent_core::{
    capability_factory, ToolBinding, ToolBindingPlugin, ToolDescriptor,
};

struct ToolConfig {
    id: String,
    call_name: String,
}

#[capability_factory(pack = "shop", config = ToolConfig)]
fn order_lookup(config: &ToolConfig) -> ToolBindingPlugin {
    ToolBindingPlugin::new(
        "shop.order-lookup",
        ToolBinding::new(
            ToolDescriptor::new(
                &config.id,
                &config.call_name,
                "查询订单状态",
                serde_json::json!({"type": "object"}),
            ),
            OrderLookup,
        ),
    )
}
~~~

factory 本身必须同步且只读取所属包配置；网络连接、文件准备、后台任务和关闭回调放在返回插件的
`install` 中。它可返回任意普通 `Plugin`，所以也覆盖第三方类型和新的插件定义。

factory 直接返回 `SkillsPlugin` 时，框架会把它识别为唯一的 Skills 运行时；它的
`only_skills(...)` 会作用于能力包合并后的清单。若返回类型经过别名或包装，增加
`skills_runtime` 标记：`#[capability_factory(pack = "shop", skills_runtime)]`。

模型操作使用明确的绑定插件，避免猜测连接或模型名称：

| 角色 | 工厂返回的插件 |
|---|---|
| Chat 连接 | `ModelConnectionPlugin`；简单单模型可直接 `impl ChatModel` |
| Embedding | `EmbeddingModelPlugin` + `EmbeddingModelService::new(ModelRef, adapter)` |
| Rerank | `RerankModelPlugin` |
| 图像、视频 | `ImageGenerationModelPlugin`、`VideoGenerationModelPlugin` |
| 语音识别、合成 | `SpeechRecognitionModelPlugin`、`SpeechSynthesisModelPlugin` |
| Realtime | `RealtimeModelPlugin` |
| Chat 选择和路由 | `ModelStrategyPlugin`、`ModelRoutingPlugin` |
| 上下文布局 | `ContextLayoutPlugin` |

`ModelRegistration` 中的 `ModelRef`、协议和能力描述由 factory 显式给出。`ModelDiscovery`
只是可查询目录：应用筛选结果后仍要显式注册，发现不会自动启用模型。会话、流和实时连接等运行时
对象仍由对应模型或存储接口在调用时创建。

内置可直接 `#[capability]` 的接口包括上下文来源、组装器、预算策略、裁剪器、会话、执行状态、
交互、长期记忆、`AgentLoop`、Chat 模型、选择策略、各类中间件、事件、输出、工具来源和策略。
`SkillProvider` 与第三方 `#[extension_point]` 也可直接声明。摘要和附件接口属于
`eliteforge-agent-context`，引入该 crate 后同样可直接声明；SQLite 存储插件继续重新导出它们的
服务和插件类型。

## Skills 是能力包成员

能力包可声明三种 Skills 来源：内嵌资源、应用已有目录，以及声明式 `SkillProvider`。选中的所有
来源会汇聚成一个 Skills 运行时，只注册一套技能清单、`skill_load` 和 `skill_read_resource` 工具。
加载、资源读取和释放会按技能名称路由回原 Provider。

推荐的发布目录约定只约束资源，不约束 Rust 源码组织：

~~~text
shop-capabilities/
├── Cargo.toml
├── src/
└── skills/
    └── order-review/
        ├── SKILL.md
        ├── scripts/       # 可选
        ├── references/    # 可选
        └── assets/        # 可选
~~~

在能力包 crate 内，用 `capability_skills!()` 把默认 `skills/` 编入发布产物：

~~~rust
use eliteforge_agent_core::{CapabilityPackBuilder, capability_skills};

pub fn shop_pack(config: ShopConfig) -> CapabilityPackBuilder<ShopConfig> {
    CapabilityPackBuilder::new("shop", config)
        .skills(capability_skills!())
}
~~~

同时添加 `build.rs`，让新增或删除资源也会触发重新编译：

~~~rust
use eliteforge_agent_core::capability_skills_build;

fn main() {
    capability_skills_build!();
}
~~~

并在 `Cargo.toml` 的 `build-dependencies` 中加入与运行时相同版本的
`eliteforge-agent-core`。自定义目录写成 `capability_skills_build!("package-skills")`。

可写成 `capability_skills!("package-skills")` 覆盖目录。开发期若资源已由应用管理，则直接传目录，
不复制也不需要资源根：

~~~rust
let pack = CapabilityPackBuilder::new("shop", config)
    .skills_directory("./company-skills");
~~~

发布产物中的内嵌资源需要应用指定准备根：

~~~rust
let agent = AgentBuilder::new()
    .capability_skills_root(app_data.join("agent-resources"))
    .capability_pack(shop_pack(config))
    .build()
    .await?;
~~~

框架将资源发布到：

~~~text
<资源根>/capability-packs/<安全包ID>/<SHA-256内容摘要>/skills/
~~~

写入先发生在同一父目录的临时目录，再原子发布。相同内容复用已有目录，不同内容并存；并发安装
会重试临时目录名并复用先完成的发布结果。Agent 关闭不会删除这个共享缓存，缓存淘汰由上层应用
负责。发现、加载和资源读取不会执行脚本，脚本执行仍受工具、`ToolPolicy` 和宿主隔离控制。

自定义 Provider 也可以直接作为包成员：

~~~rust
#[capability(pack = "shop")]
#[async_trait::async_trait]
impl SkillProvider for ShopSkillProvider {
    // catalog / load / read_resource / release
}
~~~

只要选中该包，框架会自动加入聚合 Skills 运行时；如果应用已经安装 `SkillsPlugin`，它会绑定到
同一个聚合结果。`only_skills(...)` 作用于合并后的清单。不同来源返回同名技能会在该运行第一次
建立快照时失败，并指出两个来源；同一文件系统来源内部仍保留本地目录覆盖系统目录的既有规则。

## 生命周期与失败处理

~~~mermaid
flowchart LR
    A[选择能力包] --> B[校验包、描述和依赖]
    B --> C[同步 factory 创建实例]
    C --> D[全部 Plugin install]
    D --> E[全部 Plugin ready]
    E --> F[Agent 可用]
    D --> R[失败或取消：逆序回滚]
    E --> R
~~~

`install` 注册服务、准备资源并立刻登记清理。`ready` 在所有插件安装完成后运行，可以查询完整服务
和扩展集合、绑定消费者、校验配置并登记清理；它不能新增服务。`Plugin::ready` 有默认空实现，
所以原有插件无需修改。能力包的包装插件会透传 `ready`。

安装、ready、取消或 Agent 关闭中的任一失败都会沿用容器原有的回滚逻辑。同步 factory 不承担
异步初始化；需要 I/O 的能力在 `install` 中处理，并使用 `on_cleanup` / `on_shutdown` 登记释放。

## 兼容性与边界

| 入口 | 适合的情况 |
|---|---|
| `#[extension_point]` + `#[capability] impl Trait` | 新的第三方扩展接口，或已有框架 trait 的直接实现 |
| `#[capability_factory]` | 需要包配置、工具描述、模型连接/路由、配置对象或任意普通 Plugin |
| `CapabilityPackBuilder::skills(...)` | 内嵌或目录型 Skills 资源 |
| `#[capability] impl SkillProvider` | 自定义、非文件系统的 Skills 来源 |
| `Plugin` / `PluginPack` | 已有手工装配代码、复杂异步资源或官方插件组合 |

`PluginPack`、手工 `ContextPlugin` / `ToolProviderPlugin`、`SkillsPlugin` 和旧的能力声明都继续可用。
新的声明不会自动改变工具权限、模型发现结果或操作系统隔离；这些仍由应用的配置、策略和宿主负责。

相关文档：[插件开发指南](plugins.md)、[扩展点参考](plugin-extension-reference.md)、
[Skills](skills.md)、[模型插件](model-plugins.md)。
