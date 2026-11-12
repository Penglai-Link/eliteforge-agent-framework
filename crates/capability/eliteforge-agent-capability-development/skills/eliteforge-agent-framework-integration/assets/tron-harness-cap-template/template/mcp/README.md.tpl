# MCP 服务

在 `servers.json` 中维护服务数组，构建时写入归档的 `manifest.mcp`。初始数组为空，因此包不会注册任何示例服务。

已有 HTTP MCP 服务可按以下格式配置，填写实际服务地址后再构建：

```json
[
  {
    "id": "__PACK_ID__.remote",
    "transport": "http",
    "url": "https://your-service.example/mcp"
  }
]
```

不要把凭据放进 URL。服务身份和可用性由部署环境及 Tron MCP 配置管理。

stdio 服务的 `program` 必须是包内可执行文件路径，同时将程序及依赖文件列入 `manifest.resources`。例如 `bin/my-mcp`，不能填写全局命令 `node` 或 `npx`。自行构建程序以匹配目标平台，必要时调整打包脚本。常见业务工具优先使用项目自带的 Rust backend。
