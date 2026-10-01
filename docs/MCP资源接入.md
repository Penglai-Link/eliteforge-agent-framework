# MCP 资源接入设计

## 目标与边界

本文说明 MCP 服务器提供资源和资源模板时，Framework 如何将其接入 Agent 工具调用链。资源 URI 由 MCP 服务器解释；插件不把 URI 当成本地路径，也不负责应用身份授权。

配置的 MCP 服务器声明 `resources` 能力后，插件在其命名空间中增加三个工具：

| 工具名（namespace 为 `docs`） | 参数 | 返回 |
| --- | --- | --- |
| `mcp_resources__docs__list` | 可选 `cursor` | 一页资源及可选 `nextCursor` |
| `mcp_resources__docs__templates` | 可选 `cursor` | 一页 URI 模板及可选 `nextCursor` |
| `mcp_resources__docs__read` | `uri` | 文本／二进制资源内容，保留 URI、MIME 与元数据 |

URI 由对应 MCP 服务器解释，不直接读取宿主文件。资源来自列出的 URI 或按模板构造的 URI。
未声明资源能力的服务器不暴露这三个工具；仅提供资源、没有工具能力的服务器也可以接入。
工具与远端工具一起进入 Framework 的标准调用链；服务器命名空间仍由宿主配置。

宿主也可以通过 Agent 操作读取同一连接，无需另外保存或启动客户端：

```rust,ignore
use eliteforge_agent_official_plugin_tools_mcp::{McpResourceAction, McpResources};

let page = agent.execute(McpResources {
    plugin_id: "docs-server".into(),
    action: McpResourceAction::List { cursor: None },
}).await?;

let content = agent.execute(McpResources {
    plugin_id: "docs-server".into(),
    action: McpResourceAction::Read { uri: "docs:guide".into() },
}).await?;
```

列表一次返回一页，由调用者用 `nextCursor` 续读。单页条目数受 `McpConfig::max_tools` 限制，
返回大小受 `max_result_bytes` 限制；拒绝原地重复的分页游标。
请求共享 MCP 连接的并发额度、取消、超时和关闭机制，不自动重试。
HTTP SSE 可在解码前拒绝超大帧，表现为请求错误；已解码的超大资源返回大小限制错误。
stdio／普通 HTTP JSON 解码时的峰值内存仍由 SDK 决定，返回大小限制不等同于流式内存隔离。

资源订阅和 MCP prompts 尚未提供。通用工具按需发现另见[可选工具插件](可选工具插件.md)。

## 连接生命周期与传输注意事项

宿主可使用公共 `McpClient::connect`、`discover` 和返回的 `ToolBinding`，
把执行器放入运行级 `ToolLease`。必须从连接建立起就管理取消和关闭，
包括连接成功但后续运行准备失败的路径；不要另写远端工具/资源调用协议。

`McpConfig::stdio_framing` 支持 `JsonLines`、`ContentLength` 和 `Auto`。
默认 JSON Lines 与 SDK 一致。兼容旧服务时，Auto 只允许在初始化握手失败后切换 framing，
不会重放业务工具请求；Content-Length 解析限制头部和帧大小，并保留 SDK 的协议处理。
STDIO 进程及其后代随取消/关闭回收。相关回归在 `tests/transport.rs`。
