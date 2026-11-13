# Tron 能力包模板资源

本目录是生成器和完整模板的唯一维护源，随 `eliteforge-agent-framework-integration` 技能分发。

- `scripts/tron-cap.mjs`：生成入口，需要 Node.js 22+。
- `template/`：完整项目模板，不分类型。
- `docs/pack-contract.md`：能力包约定。
- `src/bin/validate-pack.rs`：使用 Framework 安装器校验 ZIP 的辅助工具。

按上级 SKILL.md 的命令生成项目，再填写业务内容。修改生成器或模板时直接编辑本目录；使用步骤变化时同步更新上级 SKILL.md 和 metadata.version。

更新后在 Framework 根目录执行 `make development-capability-check`，并实际生成项目、校验和构建。然后发布 Framework 更新，更新 Tron 的依赖锁定版本并执行 `make be-check`，重新构建 Tron 后即可携带新模板。已生成的业务项目不会自动变化。
