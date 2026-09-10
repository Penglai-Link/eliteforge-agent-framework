# 能力包与自动装配

能力包首版已经可用。它把“业务能力的静态声明”和“某个 Agent 的业务实例”分开：
业务 crate 用 `#[capability(...)]` 声明能力，应用显式选择 `CapabilityPack`，`build()`
才为这个 Agent 创建对象并接入运行时。

`CapabilityPack` 不是 `PluginPack` 的改名。前者用于自动发现和按配置创建业务能力；
后者仍用于把已有 `Plugin` 组合成一个安装集合，官方插件包继续使用后者。两套 API 可以同时配置到同一个 Agent。

## 最小用法

业务 crate 通过 `eliteforge-agent-core` 获取能力包 API。将属性标在具体的 trait `impl` 上，
而不是结构体上；宏据此生成静态描述和接口适配代码。与 `async_trait` 一起使用时，
`#[capability]` 放在外层。

~~~rust
use async_trait::async_trait;
use eliteforge_agent_core::{
    capability, AgentError, ContextProvider, Message, RunContext,
};

pub struct ShopConfig {
    pub name: String,
}

pub struct ShopContext {
    name: String,
}

impl ShopContext {
    pub fn from_config(config: &ShopConfig) -> Self {
        Self {
            name: config.name.clone(),
        }
    }
}

#[capability(
    pack = "shop",
    id = "shop.context",
    instance = "shop.context",
    config = ShopConfig,
    factory = ShopContext::from_config
)]
#[async_trait]
impl ContextProvider for ShopContext {
    async fn context(&self, _: &RunContext) -> Result<Vec<Message>, AgentError> {
        Ok(vec![Message::system(format!(
            "你是{}的客服，请使用中文回答。",
            self.name
        ))])
    }
}
~~~

应用选择根包，并用 `include(...)` 明确选择子包：

~~~rust
use eliteforge_agent_core::CapabilityPackBuilder;
use eliteforge_agent_framework::AgentBuilder;
use shop_plugin::{AuditConfig, ShopConfig};

let agent = AgentBuilder::new()
    .capability_pack(
        CapabilityPackBuilder::new(
            "shop",
            ShopConfig {
                name: "星河商店".into(),
            },
        )
        .include(CapabilityPackBuilder::new("shop.audit", AuditConfig::default())),
    )
    .build()
    .await?;
~~~

`CapabilityPackBuilder::new(id, configuration)` 本身实现 `CapabilityPack`，不需要额外的
`build()`。`CapabilityPack`、`CapabilityPackBuilder` 和 `capability` 都由
`eliteforge-agent-core` 导出。

## 装配流程

~~~mermaid
flowchart LR
    D[业务 crate 的 trait impl / #[capability]] --> R[静态 CapabilityDescriptor / inventory 收集]
    A[AgentBuilder / capability_pack] --> S[显式选择根包和子包]
    R --> F[按已选包筛选]
    S --> F
    F --> V[预校验 ID、依赖、环和实例声明]
    V --> C[按稳定拓扑顺序调用同步 factory]
    C --> P[适配为普通 Plugin]
    P --> K[PluginContainer 安装、回滚和关闭]
    K --> G[当前 Agent 的运行时服务]
~~~

宏登记的只是静态描述和函数指针，不会创建 Provider、连接或后台任务。应用链接业务 crate
也只会让描述可发现；只有选中对应包后，`build()` 才会调用 factory。

~~~text
已链接的声明                       当前 Agent
shop.context  ───────┐
shop.audit    ───────┼─ CapabilityPackBuilder::new("shop", config)
payment.tool  ───────┘             │
                                  include("shop.audit")
                                        │
                                        ▼
                              仅 shop / shop.audit 被装配

payment.tool 的 factory 不运行。
~~~

包的子集必须由 `include(...)` 显式声明。`depends_on` 只规定已经选中的能力或显式插件的
安装前后关系，不能隐式启用另一个包。

## CapabilityPack 与 PluginPack

| 项目 | `CapabilityPack` | `PluginPack` |
|---|---|---|
| 面向对象 | 声明式业务能力 | 已创建的 `Plugin` 安装配置 |
| 应用入口 | `.capability_pack(pack)` | `.plugin_pack(pack)` |
| 成员来源 | `#[capability]` 的静态声明 | `into_plugins()` 返回的列表 |
| 配置 | 每个选中包持有自己的配置 | 包作者自行保存插件配置 |
| 实例化 | 当前 Agent 的 `build()`，按需调用 factory | 展开后由 `PluginContainer` 安装 |
| 子包 | 必须 `.include(...)` 显式选择 | 可嵌套展开插件列表 |
| 典型用途 | Shop、订单、审计等业务能力 | `EliteForgeOfficialPluginPack` 和兼容的业务组合 |

`PluginPack` 保持兼容，现有 `.plugin_pack(...)` 代码不需要迁移。首版也不会把
`EliteForgeOfficialPluginPack` 迁为能力包；它仍是官方模型、存储、交互、Skills、AG-UI、MCP 和
遥测插件的组合入口。

## 框架提供的文件系统与系统工具能力包

`eliteforge-agent-capability-tools` 提供三个可独立选择的通用能力包。它不在官方 `PluginPack`
中默认安装；文件和搜索工具必须显式提供文件系统根目录。系统命令能力包完全通用，不绑定任何
目录或项目；选中后默认允许执行所有程序，也可以配置为仅允许指定程序名。

```toml
[dependencies]
eliteforge-agent-capability-tools = "0.1.0"
```

```rust
use eliteforge_agent_capability_tools::{
    FileSystemToolsConfig, SystemCommandConfig, read_only_filesystem_pack, system_command_pack,
    write_filesystem_pack,
};
use eliteforge_agent_framework::AgentBuilder;

let filesystem = FileSystemToolsConfig::new("./data")
    .with_max_read_bytes(128 * 1024)
    .with_max_results(100);
let agent = AgentBuilder::new()
    .capability_pack(read_only_filesystem_pack(filesystem.clone()))
    .capability_pack(write_filesystem_pack(filesystem))
    .capability_pack(system_command_pack(SystemCommandConfig::new()))
    .build()
    .await?;
```

| 能力包 | 模型可见工具 | 边界 |
|---|---|---|
| `framework.filesystem.readonly` | `fs_read_text`、`fs_list`、`fs_stat`、`search_grep`、`search_glob`、`search_path` | 仅访问配置文件系统根目录下的 UTF-8 文件；拒绝绝对路径和 `..`，不跟随符号链接；读取、搜索文件、深度和结果数量均受配置上限约束 |
| `framework.filesystem.write` | `fs_write_text`、`fs_apply_patch` | 仅修改配置文件系统根目录内的文本文件；创建父目录、覆盖、追加和文本替换都属于可变更操作，应由 `ToolPolicy` 和审批流程控制 |
| `framework.system-command` | `system_exec` | 完全通用；选中能力包后可直接启动程序，参数按数组传递；Linux/macOS 可传入 `sh`，Windows 可传入 `cmd` 或 PowerShell；可配置为仅允许指定程序名；工具设置运行超时和输出上限，但不提供沙箱或进程树隔离 |

`system_exec` 不会根据操作系统自动拼接 shell 命令，也不会接受一段待解析的命令行文本。
这保证同一份工具契约可用于 Linux、macOS 和 Windows；需要 shell 内建命令时，上层应用把
对应的 shell 程序作为 `command`，并显式传递平台需要的参数。例如 Windows 使用
`{"command":"cmd","args":["/C","dir"]}`，Unix 使用
`{"command":"sh","args":["-c","ls -la"]}`。

`system_exec` 不会默认进入任何文件系统根目录或项目目录。调用提供 `cwd` 时直接使用该目录；未提供时
继承上层应用进程的当前目录。`cwd` 只是运行上下文，不是路径或沙箱边界；已允许的系统命令仍可能
读取任意数据、启动子进程或访问网络。

能力包的显式选择是第一道开关；如需收紧，使用
`SystemCommandConfig::restricted(["git", "cargo"])`。每次调用在启动进程前还会依次
经过所有 `ToolPolicy`：任一策略 `Deny` 就拒绝，未被拒绝且任一策略 `RequireApproval` 就创建审批
交互，收到明确同意后才执行。上层应用应把可选的程序限制、`ToolPolicy`、用户审批和操作系统级
隔离组合使用；命令超时或等待失败按不确定外部副作用处理，不能自动重试。

## 声明字段和工厂

| 字段 | 作用 |
|---|---|
| `pack` | 所属能力包 ID，必须与应用选择的包 ID 一致 |
| `id`（可选） | 当前 Agent 内唯一的能力 ID；省略时由包、模块、实现类型和 trait 稳定生成 |
| `instance`（可选） | 共享业务对象的键；省略时由包和实际 Rust 类型身份确定，同类型多接口默认共享对象，支持跨模块和类型别名 |
| `depends_on`（可选） | 当前已选能力或显式 Plugin 的 ID 列表；省略时为空 |
| `config` + `factory`（成对可选） | 配置类型和同步构造函数，签名为 `fn(&Config) -> Self` |

`id`、`instance` 与 `depends_on` 可按需要显式指定。默认值不依赖链接或注册顺序。
需要两份相同类型的独立对象时，给它们不同的 `instance`；同一对象实现多个接口时，给这些
`impl` 使用同一个 `instance` 和同一 factory 声明。省略 `config` 与 `factory` 时，
宏使用 `Default::default()` 创建实现，因此该类型必须实现 `Default`。
传给 `CapabilityPackBuilder` 的配置需要满足 `Send + Sync + 'static`；普通不可变配置 struct
通常天然满足这个条件。

~~~text
SharedShop
  ├─ #[capability(id = "shop.context", instance = "shop.shared")]
  │       impl ContextProvider
  └─ #[capability(id = "shop.events",  instance = "shop.shared")]
          impl EventSink

一个 Agent build：factory 一次 → 同一个 SharedShop 同时适配两个接口
两个 Agent build：factory 各一次 → 两个互相隔离的 SharedShop
~~~

共享实例的实现类型、配置类型和 factory 函数必须一致。框架按实际类型和函数身份比较，
同一函数通过不同模块路径或类型别名引用仍能共享；真正不同的 factory 会在构造前报错。
显式 `instance` 只能属于一个包；否则构建失败，不会静默选取某个实现。

同一接口需要两份同类型对象时，可以在一个 `impl` 上叠加声明，分别指定 `id` 和 `instance`：

~~~rust
#[capability(pack = "shop", id = "shop.first", instance = "shop.first")]
#[capability(pack = "shop", id = "shop.second", instance = "shop.second")]
#[async_trait]
impl ContextProvider for DefaultShopContext {
    // ...（DefaultShopContext 实现 Default）
}
~~~

每个声明有独立的宏辅助代码；上述配置会调用两次 `Default::default()` 并注册两份上下文来源。

首版可适配的 trait 包括 `ContextProvider`、`ToolProvider`、`EventSink`、
`ContextContributor`、各类 Middleware、`ToolPolicy`、`RunLifecycleHook`、
`WorkflowNode`、`RunOutputFactory` 和 `Plugin`。完整入口见
[扩展点参考](plugin-extension-reference.md)。

## 预校验、排序和生命周期

解析器先计算根包与 `include(...)` 形成的闭包，再收集其中的声明。它会在调用第一个业务
factory 之前检查：

- 空或重复的包 ID、重复选包，以及包包含环；
- 空或重复的能力 ID、与显式或框架保留 `Plugin` ID 的冲突；
- 空实例键、同一实例的 factory/config 声明冲突、跨包实例冲突；
- 缺失依赖和依赖环，包括能力与普通 `Plugin` / `PluginPack` 成员组成的混合依赖图。

通过校验后，解析器按完整依赖拓扑创建能力；能力候选按 ID 稳定排序，普通插件保留配置顺序，
跨普通插件的间接依赖同样影响 factory 顺序，所以结果不依赖 `inventory` 的迭代顺序。
工厂创建的对象先按实例身份缓存，再被各个接口声明适配为
普通插件。随后仍由现有 `PluginContainer` 安装、处理插件依赖、失败回滚和关闭回调。

生命周期和预检边界如下：

- 静态声明错误会在业务 factory 前报告，未选包的 factory 永远不执行。
- `depends_on` 可以引用应用显式配置的普通 Plugin；框架保留的内部 ID 既不能被能力占用，
  也不能作为能力依赖的满足项。
- factory 已创建对象后发生插件安装失败或取消时，沿用容器的回滚和 `agent.close()` 生命周期；
  资源型能力应在其 `Plugin::install` 中及时登记关闭回调。
- 自动创建的 `Plugin` 自身返回的动态 `dependencies()` 只能在构造后检查；需要构造前预检的
  依赖应写在宏的 `depends_on` 中。应用已经传入的普通插件则会在 factory 前读取并校验依赖。

## 首版边界

首版有意保持 factory 很小：

- 只支持同步、仅依赖所属包配置的 `&Config -> Self` factory；不支持异步 factory，也不在
  factory 中解析其他服务。
- 一个已选包的全部已声明成员都会装配；没有按成员开关、按条件发现或隐式子包选择。
- 复杂连接建立、服务解析、后台任务和资源关闭仍使用 `Plugin::install` 与现有生命周期接口。
- 官方插件包暂不迁移；首版让 `PluginPack` 和能力包并存。

需要这些超出首版边界的行为时，先保留或编写普通 `Plugin`；不要把异步 I/O、服务查找或后台
任务塞进 capability factory。

## 相关文档

- [插件与扩展开发指南](plugins.md)：选择能力包、普通 Plugin 或官方插件包。
- [扩展点参考](plugin-extension-reference.md)：接口、注册入口和数量语义。
- [Framework 架构](architecture.md)：能力声明如何进入 Agent 的服务装配。
