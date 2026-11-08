# Git tag 发布

所有 Rust workspace crate 继承根目录 `workspace.package.version`，整个仓库通过同一个 Git tag 发布。内部 crate 使用相对路径依赖，由 Cargo 从同一 Git checkout 中解析。`sdk/page` 是独立的 npm 包，不随此命令升级。

## 升级统一版本

在仓库根目录执行：

```bash
python3 scripts/release.py version 1.0.0
```

命令同步所有 workspace crate 的版本并更新 `Cargo.lock`，不创建提交或 tag。命令使用本地 Cargo 缓存；缓存不完整时会报错并恢复修改的文件。独立的 PersonAI SDK 保持固定 Git 提交，不跟随框架版本升级。

## 创建和推送 tag

完成构建验证并提交本次发布内容，确认工作区干净后执行：

```bash
python3 scripts/release.py tag
```

命令检查所有 workspace crate 的版本是否一致，并在当前提交创建 `<版本>` 格式的附注 tag，例如 `1.0.0`。已有同名 tag 时失败，不覆盖；此命令不推送到远端。

确认远端和发布内容后推送对应 tag：

```bash
git push gitee refs/tags/1.0.0
```

推送 tag 会同时上传其指向的提交，但不会更新远端分支。需要同步主分支时单独执行 `git push gitee main`。已经发布的 tag 不应移动；修复后应升级版本并创建新 tag。

## 在其他项目中引用

以下示例假设 `1.0.0` 已推送；实际使用时填写远端存在的 tag：

```toml
[dependencies]
eliteforge-agent-framework = { git = "https://gitee.com/person-ai/eliteforge-agent-framework.git", tag = "1.0.0" }
eliteforge-agent-official-plugin-storage-local = { git = "https://gitee.com/person-ai/eliteforge-agent-framework.git", tag = "1.0.0", features = ["bundled-native"] }
```

同一项目引用的框架和插件应使用完全相同的仓库 URL 和 tag，避免不同 Git 来源导致重复类型。主仓库及使用到的 Git 依赖必须允许使用者访问；`model-http` 和 PersonAI 能力包使用的 `crab-serverapi` 仍从固定 Git 提交获取。

SQLite 扩展与 tokenizer 资源保持内置，不需要拆分资源包或在运行时下载。直接克隆本仓库开发时，需满足本仓库 `.cargo/config.toml` 的 `sccache` 配置；作为其他项目的 Git 依赖时，由调用方的 Cargo 配置控制构建。
