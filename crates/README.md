# Crates 组织与插件运行方式

按职责找代码：运行时和契约在 `framework`，具体实现在 `plugin`，启用与组合入口在 `capability`。
这些目录划分源码职责；发布来源、交付方式和进程边界分别判断。

## 目录

```text
crates/
├── framework/                          契约、执行、装配与动态包基础设施
│   ├── eliteforge-agent-core
│   ├── eliteforge-agent-framework
│   ├── eliteforge-agent-plugin-core
│   ├── eliteforge-agent-context-contracts
│   ├── eliteforge-agent-capability-macros
│   └── eliteforge-agent-capability-runtime
├── plugin/                             按能力类型分类的 Rust 插件与配套实现库
│   ├── tool/
│   │   ├── eliteforge-agent-official-plugin-tools-local
│   │   ├── eliteforge-agent-official-plugin-tools-human
│   │   ├── eliteforge-agent-official-plugin-tools-agents
│   │   ├── eliteforge-agent-official-plugin-tools-mcp
│   │   ├── eliteforge-agent-official-plugin-tools-discovery
│   │   ├── eliteforge-agent-official-plugin-tools-web
│   │   └── eliteforge-agent-official-plugin-tools-media
│   ├── model/
│   │   ├── eliteforge-agent-official-plugin-model-genai
│   │   └── eliteforge-agent-official-plugin-model-external
│   ├── observe/
│   │   └── eliteforge-agent-official-plugin-otel
│   ├── protocol/
│   │   └── eliteforge-agent-official-plugin-agui
│   ├── skill/
│   │   └── eliteforge-agent-official-plugin-skills
│   ├── context/
│   │   └── eliteforge-agent-context
│   ├── memory/
│   │   └── eliteforge-agent-memory
│   └── storage/
│       ├── eliteforge-agent-official-plugin-storage-sqlite
│       ├── eliteforge-agent-state-sqlite
│       └── eliteforge-agent-sqlite-extensions
├── capability/                         随仓库提供的 Rust 能力包装配入口
│   ├── eliteforge-agent-official-capability-pack
│   ├── eliteforge-agent-capability-tools
│   └── eliteforge-agent-capability-external
├── testing/                            测试专用支持与跨 crate 装配样例
│   ├── eliteforge-agent-test-support
│   └── eliteforge-agent-capability-test-fixture
└── tools/                              可执行开发工具
    └── eliteforge-agent-capability-dev-host
```

`integration-tests/` 仍在仓库根目录，供应用侧验证公开 API 和真实模型接入。
`eliteforge-agent-plugin-kernel` 改名为 `eliteforge-agent-plugin-core`，Rust import 对应改为
`eliteforge_agent_plugin_core`。其余 Crate 名称和包 ID 保持稳定。

`crates/capability/` 存放 Rust 能力包的选择、配置和组合代码，具体工具实现在 `plugin/tool`。
它不是外部安装包仓库，也不表示其中的包只能由官方提供。
安装包由 `CapabilityPackManager` 写入宿主指定根目录下的 `packs/<id>/versions/<version>`。

外部 Agent 执行、原生 PTY、上下文准备与模型调用预算的公共接口见[宿主运行时接入](../docs/host-runtime-integration.md)。

现有工具清单、主流 Agent 对照及后续插件划分见[工具能力核查](../docs/tool-capability-audit.md)。
本轮补齐 local／MCP，并新增 discovery／web／media。业务任务清单归上层应用，LSP 和 P2 暂缓；见[实现进度](../docs/tool-capability-progress.md)。

## 三层各做什么

| 层 | 内容 | 例子 |
| --- | --- | --- |
| Framework | 定义接口、调度执行、校验参数、管理安装与关闭；提供外部包协议和 SDK | `ToolProvider`、`PluginContext`、`AgentBuilder`、`CapabilityPackManager` |
| Plugin | 实现接口，持有连接、工具或存储，负责资源清理 | `GenaiPlugin`、`SqliteStoragePlugin`、`SystemCommandPlugin` |
| Capability | 把实现和配置组合成应用可以选择的能力；工厂只创建配置和插件 | `EliteForgeOfficialCapabilityPack`、`system_command_pack(config)` |

`plugin/context`、`plugin/memory` 和 `plugin/storage` 同时包含插件及其配套实现库。
例如 SQLite 扩展加载器不是另一个 Plugin，但属于 SQLite 实现，不能混入存储无关的核心契约。
高层 Framework 门面可以依赖官方默认实现；`eliteforge-agent-core` 和插件内核保持接口职责。

## 基础 Crate 与开发工具

| Crate | 什么时候起作用 | 具体职责 |
| --- | --- | --- |
| `eliteforge-agent-plugin-core` | 构建和关闭插件容器时 | 提供 `Plugin`、服务注册表、扩展点、依赖排序、安装／ready、失败回滚和资源关闭；不包含 Agent 业务契约 |
| `eliteforge-agent-core` | 声明和装配 Agent 能力时 | 定义模型、工具、会话等 Agent 契约，提供 `CapabilityPack` 与原生声明的发现、校验和工厂调用 |
| `eliteforge-agent-capability-macros` | Rust 编译时 | 实现 `#[extension_point]`、`#[capability]`、`#[capability_factory]`，生成注册描述和适配代码；提供 Skills 嵌入与构建跟踪宏 |
| `eliteforge-agent-capability-runtime` | 安装、选择和调用动态能力包时 | 管理清单、签名、版本、实例、后端进程、RPC、远程扩展适配、页面会话与后端 SDK |
| `eliteforge-agent-capability-dev-host` | 插件开发者主动运行时 | 提供本地调试宿主、示例后端和示例 MCP 服务，验证安装、调用、页面和重配 |

`capability-macros` 是 `proc-macro = true` 的编译工具库。Rust 过程宏必须放在专门的宏 crate 中；
应用通常从 `eliteforge-agent-core` 的重导出使用这些宏。宏只生成声明和工厂，
选中包后才由原生装配流程创建实例，不会在编译时安装插件或启动能力包后端。

`capability-runtime` 是动态包基础设施库，不是默认插件集合，也不是所有原生能力的执行器。
原生 Rust 能力由 `agent-core` 解析声明、由 `plugin-core` 安装；动态包由管理器创建适配器，
接入相同生命周期，再按清单启动后端。宿主和后端可以分别使用该库提供的管理 API 和 SDK。

`capability-dev-host` 放在 `tools/`，因为它提供开发时运行的程序；
`plugin/tool/` 放的是提供给 Agent 调用的工具插件，两者职责不同。
这个 crate 设置 `publish = false`，包含 `capability-host`、`capability-backend` 和 `capability-mcp`
三个可执行文件。正常应用使用 Framework 不需要依赖或启动它；应用通过 runtime API 管理自己的安装包。
开发宿主的启动和调试方式见[示例与开发宿主](../docs/dynamic-capability-runtime.md#示例与开发宿主)。

## 官方能力如何启用

`EliteForgeOfficialCapabilityPack::new(database)` 默认装配 8 个进程内插件：

| 分组 | 默认插件 | 默认配置 |
| --- | --- | --- |
| model | Genai | 模型连接 |
| storage | SQLite | 状态、交互、会话、记忆、摘要与附件存储 |
| tool | Human、Agents、MCP | 用户问答、子 Agent／Thread 工具；MCP 初始没有服务器 |
| skill | Skills | Skills Provider 与按需读取 |
| protocol | AG-UI | 输出事件协议 |
| observe | OTel | 构建时读取宿主全局 providers |

文件、搜索、写入和命令通过同一个官方包显式配置：

```rust,ignore
use eliteforge_agent_framework::AgentBuilder;
use eliteforge_agent_official_capability_pack::{
    EliteForgeOfficialCapabilityPack,
    tools::{FileSystemToolsConfig, SystemCommandConfig},
};

let pack = EliteForgeOfficialCapabilityPack::new("agent.sqlite")
    .read_only_filesystem(FileSystemToolsConfig::new("workspace"))
    .write_filesystem(FileSystemToolsConfig::new("workspace"))
    .system_commands(SystemCommandConfig::restricted(["git"]));

let agent = AgentBuilder::new().capability_pack(pack).build().await?;
```

也可以只选择 `read_only_filesystem_pack`、`write_filesystem_pack` 或 `system_command_pack`，
或直接用 `ReadOnlyFileSystemPlugin`、`WriteFileSystemPlugin`、`SystemCommandPlugin`。
这些入口使用相同的进程内实现和 Plugin 生命周期；同一能力应选择一种入口，重复注册会报 ID 冲突。
写入包只提供写入和补丁，读取与搜索需另选只读包。

`system_exec` 默认在 10 秒内直接返回完成结果；仍在运行时返回 `taskId`。
`system_exec_task_read` 用游标续读 stdout/stderr 和状态，最长等待 10 秒；
`system_exec_task_stop` 请求停止并返回状态和后续输出。`system_exec_list` 查看命令清单，
`system_exec_task_write` 持续写入输入；`interactive` 保持 stdin，`tty` 启用 PTY。
使用方式见[本地命令工具](../docs/local-command-tools.md)。命令只能由原会话读取、输入或停止，
随 Agent 关闭清理，不跨 Agent 重建或宿主重启恢复。默认最长运行 30 分钟，可用
`with_async_after` 和 `with_timeout` 调整；每个 Agent 最多 32 条运行中的命令、256 条记录。
stdout/stderr 各保留有限的最近输出，超过容量会标记截断并返回当前起始游标。

多文件补丁、版本和搜索分页见[文件工具](../docs/local-file-tools.md)。
Web、媒体与发现插件在官方包上分别由 Cargo features `web`、`media`、`discovery` 编译，
运行时再通过 `.web(...)`、`.media(...)`、`.tool_discovery(...)` 选择安装。
它们仍在宿主进程内运行；完整配置和限制见[可选工具插件](../docs/optional-tool-plugins.md)。

## 进程边界

### 来源、交付方式和执行方式

| 维度 | 判断依据 | 作用 |
| --- | --- | --- |
| 官方／第三方 | 发布来源；安装包的签名及受信公钥 | 判断由谁发布、是否可信 |
| 内置／安装包 | 宿主 Cargo 依赖，或 `CapabilityPackManager` 安装与选择 | 决定加载路径 |
| 进程内／独立进程 | 原生 Rust 装配，或安装包的 `manifest.backend` 与贡献选择 | 决定执行方式 |

官方也可以发布外部安装包，第三方 Rust 插件也可以由宿主主动编译进去。
包 ID、目录名和 `official` 字样不是来源认证。内置代码的来源由宿主依赖选择控制；
安装包已有通用 Ed25519 验签和受信密钥管理，宿主可将签名密钥关联到发布者。
当前没有自动识别“官方身份”的分类逻辑，也不会因签名可信而改为进程内运行。

### 谁决定编译和启用

宿主构建时通过 `Cargo.toml`、Cargo features 和 Rust 代码依赖选择编译、链接的实现。
运行时通过 `.capability_pack(...)`、`.include(...)` 或 `.plugin(...)` 选择装配内容。
未选中的声明式能力不执行工厂；运行时启用能力不会临时编译或链接新 crate。

`EliteForgeOfficialCapabilityPack`、`CapabilityPackBuilder` 提供原生 Rust 组合，直接创建 Plugin。
`CapabilityPackManager::select(...)` 返回 `ManagedCapabilityPack`，按安装清单建立运行时适配器。
两条路径都使用 `.capability_pack(...)`，实际加载行为由传入的能力包实现提供。
安装包不会自动编译进宿主；要改为原生装配，宿主开发者必须添加 Rust 依赖并重新构建。

### 加载规则

| 加载方式 | 实际运行位置 |
| --- | --- |
| 宿主编译、链接的 Rust Plugin 或能力包，包括官方默认实现和第三方实现 | 宿主进程，直接调用 trait 方法 |
| 安装包声明 `backend`，且选用了扩展实现、插件或页面 | 独立后端进程，宿主通过 RPC 调用；官方与第三方使用相同规则 |
| 未声明 `backend` 的 Skills、静态页面或资源包 | 不启动能力包后端；页面由宿主挂载 |
| MCP stdio／系统命令 | 外部服务或命令进程；负责它们的 Plugin 仍在原进程内 |
| MCP Streamable HTTP | 连接远端服务，不启动本地 MCP 进程 |

动态包的 `RuntimePlugin::ready` 检查 `manifest.backend` 及选中的实现、插件、页面；
满足条件时调用 `BackendInstance::peer()`，按平台入口启动后端并建立 RPC 连接。
仅声明后端但没有选中这些内容时，ready 不启动后端；后续远程调用仍通过 `peer()` 获取连接。
代码见[启动判断](framework/eliteforge-agent-capability-runtime/src/selection.rs)和
[进程创建](framework/eliteforge-agent-capability-runtime/src/process.rs)。

```mermaid
flowchart LR
    subgraph Host[宿主进程]
        Agent[Agent 与插件容器]
        Native[编译进宿主的 Rust 插件]
        Bridge[外部能力包 RPC 适配器]
        Agent -->|直接调用| Native
        Agent --> Bridge
    end
    Bridge <-->|RPC| Backend[外部能力包后端进程]
    Native -->|按配置启动| MCP[MCP stdio 服务进程]
    Native -->|调用 system_exec| Command[命令任务进程]
```

随 Framework 提供并由宿主依赖的 Rust 插件默认在进程内装配；用户安装的可执行能力包走独立后端，
即使发布者是官方。`EliteForgeOfficialCapabilityPack` 是前一条路径的组合入口。
独立进程不自动等于操作系统沙箱。
外部包的签名、版本、实例与进程清理见[动态能力包](../docs/dynamic-capability-runtime.md)，
业务能力声明见[能力包与统一装配](../docs/capability-packs.md)。
