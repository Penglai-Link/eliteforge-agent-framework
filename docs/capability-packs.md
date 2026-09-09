# 能力包与自动装配设计

状态：设计方案，尚未接入框架。当前可用 API 见[插件与扩展开发指南](plugins.md)。
本文中的 `CapabilityPack`、`capability_pack` 和 `#[capability(...)]` 都是拟议接口。

目标是：**业务开发者实现能力并声明包归属；应用显式选择能力包；框架自动装配选中包内的能力。**
发现机制使用开源库，业务侧通过框架宏声明能力，无需理解底层注册表。

## 应用只安装包

能力包把一组可以直接使用的业务能力交给 Agent。包内可以只有 Provider，也可以包含工具、
中间件、事件监听或需要初始化资源的插件。应用安装包后，`build()` 自动接入包内全部已声明且启用的能力。

应用侧的目标写法是：

```rust
// 拟议 API：各包接收自己的配置，并在包内声明所需的子包。
let agent = AgentBuilder::new()
    .capability_pack(ShopCapabilityPack::new(config))
    .capability_pack(ObservabilityCapabilityPack::new())
    .build().await?;
```

应用负责选择包并传入配置；包作者负责提供业务实现及其创建方式。
应用无需枚举包里的 Provider，也无需调用 `ContextPlugin::new(...)` 或重复写 `.plugin(...)`。
运行、输出、问答、存储和关闭等操作继续由 Agent 实例发起。

**Cargo 依赖存在，只表示能力声明可以被链接和发现；只有选中的包才会启用。**
例如，应用同时依赖 Shop 和 Payment 两个包，但只安装 Shop：

```text
发现的声明                     当前 Agent 的选择
shop.context                         │
shop.orders                    capability_pack(ShopCapabilityPack)
shop.audit                           │
payment.context                      ▼
payment.tools                  按 shop 筛选
                                     │
                                     ▼
                               创建并装配 Shop 能力
```

Payment 的业务工厂、插件安装和后台任务都不会执行。子包必须由已选包显式声明；
能力依赖用于校验和排序，不会让所有可发现的包自动启用。

## 为什么叫能力包

| 概念 | 职责 |
|---|---|
| 能力接口 | 描述运行时行为，例如 Provider、Middleware、Policy 和 `EventSink` |
| 能力实现 | 提供具体行为，例如 `ShopContext` 返回商店说明 |
| 能力包 | 声明包含哪些能力、所需配置和依赖，供应用一次安装 |
| 插件 | 处理需要自定义的安装过程，例如建立连接、注册多项服务和登记关闭回调 |

`CapabilityPack` 表示一组可以安装到 Agent 的能力声明，公共契约不要求返回 `Vec<Plugin>`。
包可以全部由 Provider 实现组成，包作者无需为其手写 `Plugin`。

```text
ShopCapabilityPack
├─ ContextProvider
├─ ToolProvider
├─ ContextContributor
├─ RunMiddleware
├─ ToolPolicy
├─ EventSink                 事件监听
└─ Plugin                    复杂安装与资源生命周期
```

事件监听沿用框架现有的 `EventSink` 接口。`Plugin` 是其中一种高级安装方式；
`ContextPlugin`、`ToolProviderPlugin` 等简单注册适配器逐步退到框架内部。

改名需要和直接声明能力、自动装配一起实现；仅替换 `PluginPack` 的名称不会改变开发体验。

## 自动装配包含哪些步骤

```text
业务实现 Provider / Policy / Middleware / Plugin
                      ↓
#[capability(...)] 声明包归属和必要的创建配置
                      ↓
框架宏生成内部能力描述及 inventory 注册代码
                      ↓
开源注册库收集链接进应用的能力声明
                      ↓
应用选择能力包并传入配置
                      ↓
build() 筛选所选包及其显式组合的子包
                      ↓
校验声明、排序依赖、创建实例和安装资源
                      ↓
按能力接口汇总并装配
  ├─ ContextProvider 列表 → 模型输入准备
  ├─ ToolProvider 列表    → 工具执行器
  └─ EventSink 列表       → 事件分发
```

能力声明只保存元数据和工厂函数。Provider 实例、连接和后台任务在当前 Agent 的构建阶段创建，
配置由所选能力包提供。不同 Agent 可以安装同一个包并使用不同配置。

包内新增一个带宏声明的能力后，应用原有的安装代码保持不变；新增模块仍需纳入 crate 编译。
安装完成后，Provider 会在模型输入准备时调用，工具会在被选中调用时执行，事件监听会在发布事件时触发。

## 各层职责

| 层次 | 负责什么 | 对外边界 |
|---|---|---|
| 业务实现 | 返回上下文、提供工具、执行策略或处理事件 | 只实现对应能力接口 |
| `#[capability]` | 将包归属、能力接口和创建方式转换为内部声明 | 由框架公开导出，隐藏注册后端 |
| `inventory` | 收集有哪些能力声明 | 只保存元数据和工厂入口 |
| 能力包与框架 | 决定当前 Agent 启用哪些声明、使用哪些配置 | 显式选包，按包筛选 |
| `AgentBuilder::build` | 检查依赖和冲突、实例化、注册并汇总能力 | 创建当前 Agent 的服务集合 |
| 插件生命周期与 Agent | 管理资源启动、取消、关闭和失败回滚 | 复用现有资源管理机制 |

`inventory` 不承担业务实例创建、依赖注入或生命周期管理。
业务 crate 无需直接依赖 `inventory`，也无需使用 `ContextService`、`PluginContext` 或注册描述类型来声明普通 Provider。
高级插件作者仅在需要自定义安装流程时接触安装与资源接口。

更换底层收集机制时，框架保持 `#[capability]` 和能力包公共接口，调整宏生成代码及内部发现适配即可。

## Rust 内置机制核查

核查日期：2026-09-09。本地使用 Rust 1.95.0。

稳定 Rust 的 `Any` 可以检查具体类型，不能枚举一个类型实现的全部 trait，也不能用它判断任意值是否实现了某个 trait。
因此，当前 `ServiceRegistry` 按 `TypeId` 存储对象，并不能直接得到所有 `ContextProvider` 实现。
见 [Rust 标准库说明](https://doc.rust-lang.org/std/any/index.html)。

标准库另有实验性的 `try_as_dyn`，官方仍标为 nightly-only；它尝试将已知值转换为指定 trait 对象，
也没有提供扫描依赖 crate、收集工厂和创建实例的完整机制。
本方案不依赖 nightly，见 [try_as_dyn 文档](https://doc.rust-lang.org/std/any/fn.try_as_dyn.html)。

## 开源方案选择

先复用现成的发现和注册机制，再适配框架已有的依赖、装配与资源管理。

| 方案 | 已提供的机制 | 对本需求的判断 |
|---|---|---|
| [`inventory`](https://docs.rs/inventory/0.3.24/inventory/) | 在不同模块和 crate 中声明注册项，统一迭代已链接的注册项 | 首选后端，已验证基本收集机制；注册代码由框架宏生成 |
| [`linkme`](https://docs.rs/linkme/0.3.37/linkme/) | 通过链接器将分散声明的静态项合成切片 | 可替代的收集机制；需结合目标平台验证 |
| [`bevy_reflect`](https://docs.rs/bevy_reflect/0.19.1/bevy_reflect/) | 类型元数据、显式声明的 trait 反射和类型注册 | 如果需要运行时反射再考虑；当前按接口收集工厂不需要字段反射 |

建议先采用 `inventory` 收集能力声明，复用当前容器进行实际安装。
`inventory` 已覆盖跨 crate 注册，框架无需自行实现链接段扫描或进程初始化钩子。
它的注册表迭代顺序没有保证，因此业务安装顺序必须由框架按依赖和稳定规则确定。
见 [inventory 注册与迭代规则](https://docs.rs/inventory/0.3.24/inventory/)。

`inventory` 在进程初始化时登记注册项；登记内容应限于静态描述与工厂指针，
不要在登记表达式中创建业务实例、连接或后台任务。
其支持平台和初始化机制见 [inventory 项目说明](https://github.com/dtolnay/inventory#how-it-works)。

## 业务侧通过宏声明能力

框架提供 `#[capability(...)]`，由它生成 `inventory::submit!` 和内部注册描述。
直接编写底层注册代码不作为业务开发方式；宏封装是首版 API 的组成部分。

### 无配置的 Provider

下面是目标写法。宏从 `impl` 中取得能力接口与实现类型，开发者只声明所属包：

```rust
// 拟议的 capability 宏；其他接口沿用当前框架定义。
use async_trait::async_trait;
use eliteforge_agent_core::{capability, AgentError, ContextProvider, Message, RunContext};

#[derive(Default)]
struct ShopContext;

#[capability(pack = "shop")]
#[async_trait]
impl ContextProvider for ShopContext {
    async fn context(&self, _: &RunContext) -> Result<Vec<Message>, AgentError> {
        Ok(vec![Message::system("你是商店客服，请使用中文回答。")])
    }
}
```

安装 `ShopCapabilityPack` 后，构建过程才使用 `Default` 创建 `ShopContext`，并把它加入上下文来源列表。
框架宏负责生成能力角色到服务注册的映射，业务实现不需要调用注册 API。

### 为什么建议标记 impl

Rust 属性宏接收被标记语法项的 token。标记 `struct ShopContext` 时，输入中没有其他位置的
`impl ContextProvider for ShopContext`，不能直接据此知道它实现了哪些能力接口。
见 [Rust 属性宏说明](https://doc.rust-lang.org/reference/procedural-macros.html#the-proc_macro_attribute-attribute)。

因此，本方案建议标记具体的 `impl`，从声明本身取得接口信息，并生成由编译器校验的转换代码。
这样保留“实现能力 + 声明属于哪个包”的开发方式，也无需另外列出接口。
若将宏放在 `struct` 上，则需要额外的接口声明或配套的 `impl` 标记，不能把一个孤立的结构体标记描述成完整反射。
`capability` 与 `async_trait` 的组合顺序及诊断需要在宏实现阶段验证。

### 带配置的 Provider

需要配置时，包的 `new(config)` 保存当前 Agent 使用的业务配置，能力声明指定一个业务工厂。
例如将上述声明扩展为：

```rust
// 属性参数示意，应用于对应的能力 impl；factory 由业务类型提供。
#[capability(pack = "shop", factory = ShopContext::from_config)]
```

`ShopContext::from_config` 接收声明类型的包配置并创建实现。宏生成读取配置和调用工厂的适配，
业务工厂只处理自己的配置，普通 Provider 不必接触通用容器。
无工厂参数时要求实现 `Default`；不能默认构造、缺少配置或工厂返回类型不匹配时，必须明确报错。
异步创建资源和依赖其他服务的高级工厂契约需与安装生命周期一起定义。

能力 ID 可以由包、实现类型和能力接口生成；跨能力依赖需要稳定名字时，允许显式指定 `id`。
同一实现类型声明多个接口时，应复用同一包实例中的业务对象，再分别注册接口视图。
需要同类型的多个独立对象时，使用显式实例标识；配置或工厂声明冲突应报错，不能静默重复创建。

这一宏只封装框架领域的声明和适配规则。底层发现继续使用开源库，不自行开发通用反射或扫描系统。

## 自动装配必须保留的规则

| 问题 | 设计要求 |
|---|---|
| 应用链接了多个包 | 仅激活应用选择的包及其声明的子包；其他包的工厂不执行 |
| 增加一个标记了 capability 的实现 | 包启用时自动纳入，应用不追加成员注册代码 |
| 包里只有 Provider | 可以安装，不要求包作者额外实现 `Plugin` |
| 同一接口有多个实现 | 支持多贡献的接口收集为列表；唯一角色冲突时报错 |
| 多个包提供同一能力 ID | 报告冲突及来源，不依赖收集顺序覆盖 |
| 能力依赖其他服务 | 声明依赖，按拓扑顺序创建；安装期解析只保证前置依赖已就绪 |
| 消费者需要完整 Provider 列表 | 在全部相关安装完成后汇总，不能把安装途中的部分列表当成最终结果 |
| 同一个包用于多个 Agent | 工厂按 Agent 创建实例，配置及生命周期分别管理 |
| 同一实现类型提供多个能力接口 | 默认共享该包实例内的业务对象；不因多个接口标记重复初始化资源 |
| 安装失败或取消 | 复用容器的回滚和关闭回调，已取得资源必须被释放 |
| 修改成员开关和配置 | 在构建前决定；默认启用的成员无需应用再次逐项登记 |

能从声明中确定的重复 ID、缺失依赖和唯一性冲突，在创建资源前检查。
自定义插件安装时动态产生的服务冲突仍由容器检查，并触发回滚。

## 当前实现与目标的差别

当前 `PluginPack` 已能让应用一次安装多个成员；`build()` 已会将注册好的 `ContextService`
汇总为 Provider 列表。还需补充框架能力宏、自动收集声明、按包选择实例和隐藏注册封装的入口。

迁移时先实现能力宏和自动装配，再迁移官方包及应用示例。
业务侧的 `ContextPlugin` 等注册适配器逐步退到内部，高级 `Plugin` 的依赖与资源管理能力保留。
以上 API 尚未实现，生产 workspace 尚未添加发现后端或宏依赖。

## 开源机制验证

在仓库外建立了最小验证项目，使用 Rust 1.95.0 和 `inventory` 0.3.24，
分别运行普通构建与启用 LTO 的发布构建。验证了：

- 不同模块中的两个 Provider 工厂可以从业务包 crate 自动收集到应用，应用无需维护成员清单。
- 已链接但未选择的另一个包可以被发现，其工厂不会执行。
- 收集声明不会创建 Provider；构建时才调用工厂。
- 同一个包可以根据两份配置分别创建实例，排序由框架逻辑明确指定。

这是对开源收集机制的验证，使用简化的 Provider 接口，并未验证 `#[capability]` 宏。
实际框架的异步依赖安装、冲突回滚和 Agent 关闭仍需在后续接入时验证；其他目标平台尚未实测。
