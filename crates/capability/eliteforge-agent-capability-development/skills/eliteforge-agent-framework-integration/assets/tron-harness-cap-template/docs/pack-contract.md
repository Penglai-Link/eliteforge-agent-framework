# 能力包约定

`manifest.json` 位于 ZIP 根目录，使用 `manifest_version: 1`。版本遵循 SemVer；模板默认 `framework: ^1.0.0`，Rust 依赖固定到 `e4e53e9072160925c178f81bf7989cefb07c018f`。每次升级 Framework 都应重新校验生成包。

```mermaid
flowchart LR
    T[完整模板和包 ID] --> S[独立项目源码]
    S --> C[静态校验与可选 Rust 编译]
    C --> Z[manifest + 内容 + checksums.json]
    Z --> I[Tron 导入或能力中心分发]
    I --> E[启用与 Agent 资源授权]
```

## 文件和分发

- 包 ID 使用小写字母、数字、点和连字符；ID 确定包身份，升级保持不变。
- `resources` 列出文件，不能填写目录。`skills` 列出技能根目录。
- 打包器只收集声明的资源、技能目录及原生程序，不打包项目源码、凭据、缓存或整个工作目录。声明路径不得越界或经过符号链接。
- `checksums.json` 为除自身之外每个文件记录 SHA-256。输出为未签名的开发归档；它可经 Tron 当前导入流程校验，不代表具有发布者签名。
- 原生 ZIP 只适用于文件名和 manifest 标注的当前操作系统、架构。分别在目标平台构建；不要将 macOS 程序重命名为 Windows 或 Linux 程序。WSL2 使用 Linux 构建，操作 Windows 桌面需单独设计桥接。

## 页面

页面声明写入 `tron/pages.json`，菜单写入 `tron/navigation.json`。导航类型使用 `PackPage`，`pageCode` 对应本包页面 ID。启用后在 Tron 导航领域选择本包。

页面仅有静态 HTML/CSS 和单个 IIFE 脚本。通过 `window.tronPage.onContext` 订阅语言和外观；颜色使用宿主 `--color-*` 变量。没有父页面 DOM、令牌或任意 API 访问权。不要生成远程脚本、CDN、ES module、动态 import、表单或嵌套 iframe。资源必须是本地相对路径，每页最多 256 个文件、16 MiB。

## 导航编码与升级

菜单和领域编码均须原样匹配 `^[A-Z0-9_]{1,64}$`。合法示例为 `COM_EXAMPLE_ASSISTANT_OVERVIEW`；`com.example.assistant.overview`、`assistant-overview`、` overview ` 都不合法。Tron 会比较规范化结果与原值，大小写或首尾空白不同也会拒绝导入。

`layouts` 的键对应本包领域编码，`menuCode` 对应本包菜单编码，非空 `parentMenuCode` 指向同一布局中的 `Group`。同一布局内菜单不可重复，`position` 是非负整数，同一父节点下不能重复；深度不得超过目标 Tron 的配置。领域和菜单 `name` 均需非空 `zh-CN`、`en-US`。

`PackPage.pageCode` 对应本包页面 ID，页面 ID 使用 `^[A-Za-z0-9_-]{1,64}$`，不要将菜单编码规则套到页面 ID。`Internal.pageCode` 引用宿主已有页面编码，例如 `agents.index`。声明 `tron/navigation.json` 和所需 `tron/pages.json` 到 manifest 的 `resources`，否则宿主不会读取这些声明。

修复已发布包时保持包 ID，递增 manifest 的语义版本，重新生成 ZIP 和校验和；仅改 ZIP 文件名不算升级。若需修复旧编码，核对已安装布局与全部引用。实际导入还会检查与既有领域/菜单的归属冲突；Framework 安装器通过不代表 Tron 导航校验通过，启用后菜单展示与跳转仍需实机确认。

## 工具和 MCP

原生 backend 通过 Framework SDK 的 `TypedBackend`、`BackendResource` 和 stdio 协议实现，stdout 只用于协议或显式的 `catalog` 命令。示例工具是无副作用的 echo。增加业务工具时同时更新目录描述和执行实现；需要宿主服务时复用 SDK 的上下文，不另建 Agent 循环或绕过权限。

`manifest.tools` 是展示元数据，不能单独提供执行器。原生构建从程序的 `catalog` 输出生成目录，避免手工维护两份工具定义。

MCP 声明维护在 `mcp/servers.json`，初始为空数组；打包器将其合并到归档 manifest。提供的示例使用 HTTP 服务。若自行添加 stdio MCP，`program` 必须是包内可执行程序路径，并在 `resources` 中列出；不能直接写 `node`、`npx` 或主机绝对路径。外部服务的连通性、身份配置和工具权限在 Tron 中确认。

## 校验边界

`make lint` 检查模板约定、声明路径、技能和静态页面引用；`make build` 编译并打包。打包不会启动 Tron、联系 MCP 服务或发布文件。需要验证宿主兼容性时，在模板仓库执行 `make validate-pack PACK=/absolute/path/package.zip`，该命令调用真实 Framework 安装器并使用临时目录，不启用包。最终页面展示、授权和真实业务调用仍需在 Tron 中验收。
