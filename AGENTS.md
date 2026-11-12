# AGENTS.md

## 共识

当用户说开发完成时，做以下事项：
- 提交所有改动
- 合并到主分支
- 推送主分支
- 清理分支和worktree

## 对接技能维护

[Framework 对接技能](crates/capability/eliteforge-agent-capability-development/skills/eliteforge-agent-framework-integration/SKILL.md) 由 Framework 的 `eliteforge.agent-development` 能力包统一维护，根目录 `skills/` 不保留副本。修改公开接口、装配方式或使用示例时，同步该技能受影响的说明、references 和模板，更新 `metadata.version` 并运行 skill-creator 校验。技能位于本仓库上述能力包目录。
