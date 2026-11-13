# EliteForge Agent Development 能力包

包 ID：`eliteforge.agent-development`。本包位于 Framework 的 `crates/capability/eliteforge-agent-capability-development`，三个开发技能在本目录统一维护：

- [Framework 对接](skills/eliteforge-agent-framework-integration/SKILL.md)：Rust Agent、插件、工具和能力包开发，包含订单助手模板和 Tron 能力包生成器。
- [Agent UI 对接](skills/eliteforge-agent-ui-integration/SKILL.md)：React/Vue npm 包、宿主 adapter/transport 和运行控制。
- [Agent UI 主题](skills/eliteforge-agent-ui-theme/SKILL.md)：公共主题变量、样式挂点和 React/Vue 外观。

内部开发者、外部开发者和 Tron 中的 Agent 使用相同的开发技能。新建 Tron 能力包时，Agent 先执行技能内的 `assets/tron-harness-cap-template/scripts/tron-cap.mjs` 生成完整骨架，再填写业务内容，最后校验与构建。模板用于减少模型重复编写基础代码的成本，不另设开发技能或能力包。

Tron 将整个 `skills/` 目录编译进后端，默认安装并启用本包。启动时按内容哈希展开技能资源，登记到技能中心；无需手动导入，也无需 Framework/UI 源码 checkout。禁用后，技能中心保留包归属记录，三个技能停止提供；Agent 使用权限沿用技能中心的授权规则。

```mermaid
flowchart LR
    S[本包 skills：说明、references、模板] --> B[编译嵌入 Tron 后端]
    B --> C[启动时登记技能中心]
    C --> P{能力包已启用且 Agent 获得授权}
    P -->|是| R[运行时读取技能与资源]
    P -->|否| D[技能不可用]
```

Framework 或 Agent UI 接口变化时，在对应代码改动中同步本目录，更新受影响技能的 `metadata.version`，并核对 `agents/openai.yaml`。Framework 根目录 `skills/` 与 Agent UI 仓库只保留维护指引，不保留技能副本。引用中的 Framework/UI 源码路径均相对于各自仓库根目录；npm 和 Git 版本基线仍以技能正文为准。

Tron 生成器和完整模板直接维护在本技能的 `assets/tron-harness-cap-template/`，不再维护独立模板项目。修改后发布 Framework 更新，由 Tron 更新依赖锁定版本并重新构建。技能分发必须保留完整目录，不能只复制 `SKILL.md`。

在 Framework 根目录执行 `make development-capability-check` 校验技能并编译能力包；Tron 通过能力包依赖完成默认装配，在 Tron 根目录执行 `make be-check` 检查宿主接入。不需要新增测试代码。
