# 开发约定

- 项目骨架已由生成器创建；按用户需求填写业务内容，保留完整目录，不重新编写基础模板。示例 echo 工具不代表业务开发完成。
- 运行 `make install` 准备依赖，修改后执行 `make lint` 和 `make build`。
- 只有用户要求时才编写测试代码。不要将调试凭据放进能力包。
- Framework Git 提交固定在 native/Cargo.toml，升级时确认目标 Tron 的兼容版本。
- 新增资源必须登记 manifest.json；MCP 声明只维护 mcp/servers.json。
- stdout 用于 Framework 协议，原生程序的日志输出到 stderr。
- 只在授权的工作目录内执行文件和命令操作；导入、启用、授权、发布遵循用户任务范围。
