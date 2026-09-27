# 工具补齐进度

范围：执行 [工具核查](tool-capability-audit.md) 中的 P0/P1，排除业务任务清单、LSP 和全部 P2。
运行命令的 taskId、命令清单、输入、读取与停止属于命令生命周期，仍在范围内。
不新增 `tools-planning`；Skills 管理、历史会话检索、浏览器、Notebook、监测推送与调度暂缓。

| 工作项 | 状态 | 完成证据 |
| --- | --- | --- |
| 命令持续 stdin、PTY、命令清单、受管子进程清理 | 已实现，macOS 行为验证通过 | `capability_packs` 19 项集成测试，含真实 10 秒交接、PTY、stdin、会话隔离及子进程清理 |
| 多文件补丁、版本冲突、diff、失败回滚 | 已实现，行为验证通过 | 增删改／移动、预览、版本冲突、非法路径、大小限制；两项故障注入验证回滚和外部修改保留 |
| 完整 glob、ignore／exclude、搜索模式、分页与取消限制 | 已实现，行为验证通过 | glob／brace／字符范围、ignore、排除、纯文本／计数、文件变化后的快照续读及会话隔离 |
| MCP 资源列表、模板及读取 | 已实现，协议验证通过 | `transport` 8 组场景：HTTP／stdio、分页、文本／blob、限额、取消／超时、Agent 操作与工具注册 |
| 通用工具按需发现、Schema 加载与策略链 | 已实现，4 项行为测试通过 | 新 run 隐藏、猜测名称拒绝、私有工具、ID 冲突、Schema 校验、ToolPolicy、撤销与租约关闭 |
| 可配置 Web 搜索与抓取插件 | 已实现，2 组本地协议测试通过 | Brave 请求、来源 URL、HTML 正文、重定向域名检查、大小限制、超时、取消、HTTP 错误 |
| 本地图片／PDF 读取插件 | 已实现，2 组文件场景和等待超时测试通过 | PNG 原生媒体块、像素与路径限制、取消、PDF 指定页、空页、截断和损坏文件 |
| 官方能力包装配、使用文档与整体回归 | 已完成本轮验证 | 三个独立 Cargo features 和运行时显式选择；装配测试确认编译不自动启用插件，默认仍为 8 个插件 |

本轮 P0/P1 范围已完成。2026-09-27 Tron 全量接入复验（macOS arm64）：

- Workspace 每个测试目标启用 `--all-features`：422 项常规测试通过、0 失败；包含独立插件进程的 18 项行为测试。
- 独立协议程序：MCP 8 组、崩溃恢复 4 组通过。
- Framework `--no-default-features` 的全部目标和文档测试：142 项通过。
- 1 个宏文档片段原本就是带省略号的示意代码，保持 `ignore`；未用它替代宏的行为测试。
- `cargo fmt --all -- --check`、`git diff --check` 通过。

测试记录分别在 `/tmp/framework-final-tests/results.json` 和 `/tmp/framework-nodefault-tests/results.json`。
磁盘容量不足以同时保留全部测试可执行文件，因此按 Cargo metadata 的 workspace 成员和测试目标顺序执行，
每个目标结束后释放它的可执行产物。`--all-features` 未关闭；失败目标修复后重新执行。
插件包压缩和进程场景使用 `RUST_TEST_THREADS=2` 控制资源占用。
异步观察者回归改用始终不返回的观察者，断言主运行仍能完成且 Agent 能正常关闭；
移除原来依赖“主运行 20 秒、观察者睡眠 60 秒”的机器速度假设，仍保留 60 秒的防挂死上限。
命令使用 `--locked --offline`，设置 `CARGO_BUILD_JOBS=2`、`CARGO_PROFILE_DEV_DEBUG=0`、`CARGO_PROFILE_TEST_DEBUG=0`、`CARGO_INCREMENTAL=0`。
私有 Cargo 镜像连接失败，验证期间用单次命令的 `--config 'source.panamax-sparse.registry="sparse+https://rsproxy.cn/index/"'` 切换镜像，未修改全局 Cargo 配置。
改动保存在 `feature/clouds3n/framework-capability-layout` 工作区。

## 当前验证边界

命令行为测试运行于 macOS arm64；Windows Job Object／ConPTY 与 Linux 尚未做原生运行验证。
命令清单只管理执行记录，业务待办清单、LSP 与 P2 均未实现。
使用文档：[本地命令](local-command-tools.md)、[文件编辑与搜索](local-file-tools.md)、[MCP 资源](mcp-resources.md)、[可选工具插件](optional-tool-plugins.md)。

此前 `bundled-native` 校验失败的原因是词典清单记录的哈希与发布包实际字节不一致。
2026-09-27 已逐项核对上游 v0.7.1 发布包，修正 `PROVENANCE.json`，并用 `.gitattributes` 禁止资源文本换行转换。
校验仍然强制执行，未通过跳过校验或关闭 feature 绕过。Tron 全量接入复验已通过每个 workspace 测试目标的 `--all-features`。

Tron 宿主扩展见 [宿主运行扩展](host-runtime-integration.md)，多目录、动态 cwd 和附件引用见 [文件工具](local-file-tools.md)。
