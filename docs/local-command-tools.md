# 本地命令与命令清单

通过官方能力包的 `.system_commands(config)` 或 `SystemCommandPlugin` 显式启用。
插件在宿主进程内管理命令，不隐式调用 Shell；要使用管道等语法，需显式执行宿主允许的 Shell。

| 工具 | 用途 |
| --- | --- |
| `system_exec` | 启动程序，默认等待最多 10 秒；未完成则返回 `taskId` |
| `system_exec_list` | 查看当前会话的命令清单；`includeCompleted: true` 包含保留的已结束记录 |
| `system_exec_task_read` | 按 stdout／stderr 字节游标续读，查询运行状态；`waitMs` 最多 10 秒 |
| `system_exec_task_write` | 向已打开的 stdin 写入；`closeStdin: true` 在写入后发送 EOF |
| `system_exec_task_stop` | 请求停止受管命令及其进程组，返回状态和输出 |

启动交互式命令：

```json
{"command":"python3","args":["-u","script.py"],"interactive":true}
```

向返回的命令 ID 写入：

```json
{"taskId":"cmd-实际返回的ID","data":"下一行输入\n","waitMs":1000}
```

需要终端行为时在启动参数中加入 `tty: true`，可设置 `rows`／`cols`（默认 24／80，各为 1–1000）。
PTY 自动保持输入打开，stdout 和 stderr 合并在 `stdout`；`stderr` 为空。
普通命令默认不保留 stdin；只传初次 `stdin` 时，写完会关闭，与此前行为一致。

命令返回 `pid`、`tty`、`stdinOpen` 和运行状态；命令清单还返回记录年龄 `ageMs`。
每次输入最多 64 KiB，输入队列有界。写入失败或等待超时可能已交付部分内容，返回不确定错误，
调用者应先读取输出判断进度，不能无条件重发。游标单位是字节，截断时同时返回保留范围的起始游标。

命令清单只包含当前会话的命令，其他会话无法读取、输入或停止它们。
每个 Agent 最多 32 个运行命令、256 条记录，旧的已结束记录按创建顺序淘汰。
默认最长运行 30 分钟；记录不跨 Agent 重建或宿主重启恢复。

Unix 命令各自建立进程组，PTY 建立独立会话；停止、超时、主命令退出、Agent 关闭都会清理受管组。
Windows 使用 PTY 后端和 Job Object 管理。进程组和 Job Object 是生命周期管理措施，不构成 OS 沙箱。
主动脱离 Unix 进程组的程序不在该清理范围内。实际平台验证范围见[进度与证据](tool-capability-progress.md)。
