# __PACK_ID__

这是由 `tron-harness-cap-template` 生成的完整 Tron 能力包项目，初始版本为 `__PACK_VERSION__`。项目可以独立维护，不依赖模板仓库。

骨架生成后，先根据用户需求填写业务工具、页面、技能说明、导航、MCP 配置和资源，再校验与构建。示例 echo 工具和概览页面仅用于起步，不能当作业务需求已完成。

```text
manifest.json       包身份、版本与贡献声明
skills/             Agent 技能和参考资料
native/             Rust 原生工具及 Framework backend 入口
ui/                 隔离页面 HTML、CSS、IIFE 脚本
tron/               页面与导航声明
mcp/                MCP 服务配置及接入说明
resources/          随包分发的资源
scripts/            校验、编译和打包脚本
```

需要 Node.js 22+、Rust 2024 工具链及当前平台的编译工具。在项目根目录执行：

```sh
make doctor
make install
make lint
make build
```

没有 GNU Make 时，可分别执行 `node scripts/project.mjs doctor`、`install`、`lint`、`build`。支持 macOS、Linux、Windows 原生环境；跨平台分发需在各目标平台构建。保留首次安装生成的 `native/Cargo.lock`，后续使用锁定依赖。

将 `output/__PACK_ID__-__PACK_VERSION__-<os>-<arch>.zip` 导入 Tron「设置 → 能力包」，启用后分配技能和工具权限。在导航领域选择本包即可打开概览页面。示例工具 `__TOOL_NAME__` 回显 `message` 字符串，可作为业务工具开发的起点。

MCP 配置位于 `mcp/servers.json`，初始为空数组，填写真实配置后才会进入归档的 `manifest.mcp`。修改业务页面、技能和工具时同步清单；新增资源逐文件列入 `resources`。需要附带业务程序时放在包内并明确声明路径。

```mermaid
flowchart LR
    S[修改源码及声明] --> C[校验和编译]
    C --> Z[带 SHA-256 校验清单的 ZIP]
    Z --> I[Tron 导入]
    I --> E[启用和授权]
    E --> A[Agent 技能和工具 / 隔离页面]
```

版本升级需同时修改 `manifest.json` 和 `native/Cargo.toml`。`make build VERSION=x.y.z` 仅接受与清单相同的版本，不改写源码；`SKIP_TESTS` 接受 true/false，本模板未配置测试。已有同名 ZIP 不会被覆盖，重新构建前移动归档或执行 `make clean`。`make deploy` 在没有配置发布目标时明确报错。

当前 ZIP 未签名，供 Tron 的开发包导入流程使用。打包只收集声明资源和编译产物，不会自动发布到能力中心。页面只使用宿主提供的语言和主题上下文，没有任意 API、工具或登录凭据访问权。
