# eliteforge-agent-capability-remote-access

Tron 自有的原生能力包，把 remote 访问的 Agent 侧控制能力复用给其他 PersonAI 产品。
宿主负责监听器、配置存储、配对令牌的生成与持久化；本包只提供工具参数校验、运行上下文检查、
工具输出脱敏和内嵌技能。Tron 的接入由 lead 后续完成，本 crate 不改动主后端、前端、Electron 或移动端。

```text
crates/capability/eliteforge-agent-capability-remote-access/
├── Cargo.toml                         独立 workspace；包名 eliteforge-agent-capability-remote-access 0.1.0
├── Cargo.lock                         由本 crate 的 cargo check 生成
├── build.rs                           capability_skills_build!() 跟踪 skills/ 变化
├── src/
│   ├── lib.rs                         PACK_ID、RemoteAccessCapability、skill_source / skill_names
│   ├── host.rs                        RemoteAccessHost 宿主契约
│   └── tool.rs                        remote_access 工具、确认校验、输出脱敏
├── skills/remote-access/SKILL.md      内嵌技能
└── README.md
```

## 对外契约

| 项目 | 值 |
| --- | --- |
| 能力包 ID | `framework.remote-access` |
| 工具插件 ID | `framework.remote-access.tools` |
| 工具 ID | `framework.remote-access.manage` |
| 模型可见工具名 | `remote_access` |
| 内嵌技能 | `remote-access` |
| Framework 风险声明 | `ToolOperationKind::Command` |
| 参数 schema | `{ "action": "status" \| "pairing" \| "rotate" \| "enable" \| "disable", "confirm": boolean? }` |

```rust
let capability = RemoteAccessCapability::new(host); // Arc<dyn RemoteAccessHost>
let pack = capability.capability_pack();           // CapabilityPackBuilder<()>
```

`capability_pack()` 只组装声明：注册 `ToolProviderPlugin`（`framework.remote-access.tools`）并内嵌
`skills/`。在 Framework 选中该包之前不会调用任何宿主方法，也不会租出工具。

## 工具行为

| action | 宿主方法 | 需要 confirm | 说明 |
| --- | --- | --- | --- |
| `status` | `status()` | 否 | 远程访问配置与监听器状态 |
| `pairing` | `pairing()` | 否 | 只返回地址、产品、节点、是否已配置令牌和查看二维码的提示 |
| `rotate` | `rotate_token()` | 是 | 轮换令牌，既有客户端立即失效 |
| `enable` | `set_enabled(true)` | 是 | 开启远程访问 |
| `disable` | `set_enabled(false)` | 是 | 停用远程访问 |

`rotate`、`enable`、`disable` 未传 `confirm: true` 时直接返回可读错误，不调用宿主，不执行破坏性操作。
`confirm` 为 `null` 或省略都视为未确认。

每次调用返回一个 JSON 字符串，并同步填入 `ToolOutput.structured`：

```json
{"ok": true, "action": "status", "data": {"enabled": true, "tokenConfigured": true}}
```

```json
{"ok": false, "action": "rotate", "data": {"error": "rotate 会改变远程访问状态或配对令牌，必须先获得用户明确同意并传 confirm: true。", "requiresConfirmation": true}}
```

失败时 `ToolOutput.is_error` 为 `true`；宿主返回错误信息时原样放入 `data.error`。

### 令牌脱敏

任何 action 的输出都经过同一套脱敏，然后才交给 Agent：

- `token`、`secret`、`password`、`credential`、`authorization`、`jwt`、`bearer` 等字段的非布尔值替换为 `***`；
  `tokenConfigured` 这类布尔字段保留，用于回答“是否已配置”。
- `pairingUri`、`tokenUri`、`qrCode`、`qrUrl` 等字段整体丢弃，不进入 Agent 上下文。
- 字符串中的 `token=` / `access_token=`（含 `%3D` 编码）查询参数值替换为 `***`。
- `pairing` 的 `data` 只投影以下安全字段（宿主提供哪些就返回哪些）：
  `productCode`、`productName`、`nodeId`、`baseUrl`、`publicBaseUrl`、`machineName`、
  `enabled`、`tokenConfigured`、`allowInsecureHttp`，并附加
  `tokenVisibleToAgent: false` 和 `instructions: "请在桌面端设置页查看配对二维码或令牌；Agent 不会显示、复述或转发令牌。"`
- 脱敏只能识别结构化 token 字段和 `token=` 参数。宿主不应把令牌拼进自由文本；工具层无法可靠识别所有自由文本中的秘密。

## RemoteAccessHost

宿主实现 `RemoteAccessHost`（`async_trait`，`Send + Sync`），负责实际配置、持久化、令牌生成和监听器状态：

```rust
#[async_trait]
pub trait RemoteAccessHost: Send + Sync {
    async fn status(&self) -> Result<serde_json::Value, String>;
    async fn pairing(&self) -> Result<serde_json::Value, String>;
    async fn rotate_token(&self) -> Result<serde_json::Value, String>;
    async fn set_enabled(&self, enabled: bool) -> Result<serde_json::Value, String>;
}
```

返回 JSON 建议使用 camelCase。`status` 推荐字段：`enabled`、`active`、`restartRequired`、
`tokenConfigured`、`productCode`、`productName`、`nodeId`、`machineName`、`baseUrl`、
`publicBaseUrl`、`port`、`allowInsecureHttp`。`pairing` 至少提供 `baseUrl`、`productCode`、
`nodeId`、`tokenConfigured`。`rotate_token` 与 `set_enabled` 返回操作后的状态快照。
额外字段会被保留（`status`/`rotate`/启停）或忽略（`pairing` 只投影上面列出的安全字段）。

约定：

- 错误用 `Err(String)` 返回面向用户的可读原因，不要包含令牌或它的片段。
- 不要依赖能力包写配置或轮换令牌；能力包没有持久化能力，只转发调用。
- 再次强调：宿主最好根本不返回 token 和配对 URI。返回了也会被脱敏，但多一层暴露面。

## 其他 PersonAI 产品接入

1. **依赖**：本包 `publish = false`，不发布到 crates.io。workspace 内使用 path 依赖；跨仓库使用
   git 依赖（Tron 仓库）或构建流水线同步源码。例如：

   ```toml
   [dependencies]
   eliteforge-agent-capability-remote-access = { path = "../Tron/crates/capability/eliteforge-agent-capability-remote-access" }
   ```

2. **实现宿主**：用产品自己的配置服务实现 `RemoteAccessHost`。远程端口、HTTPS、FRP/反代和
   令牌生命周期由产品负责，参考 `skills/remote-access/SKILL.md`。

3. **装配能力包**：交给 `FrameworkBuilder`，并配置技能资源根：

   ```rust
   let pack = RemoteAccessCapability::new(Arc::new(ProductRemoteAccessHost::new(settings)))
       .capability_pack();
   let framework = FrameworkBuilder::new()
       .capability_skills_root(resource_root)
       .capability_pack(pack)
       .build()
       .await?;
   ```

4. **技能中心登记**：产品若有自己的技能资源同步流程，把该包的
   `CapabilityPack::registry_skill_sources(&pack)` 加入启动和安装/卸载后的同步；技能名为
   `remote-access`。否则用户看不到技能，但工具仍可用。

5. **目录与权限**：产品的能力包目录、导航页和工具权限页需要登记包 ID、工具 ID 和工具名；
   若产品用静态风险表，建议把 `framework.remote-access.manage` 的查询操作标为低风险，
   `rotate`/`enable`/`disable` 标为高风险。能力包内部的 `confirm: true` 校验不能替代产品的
   ToolPolicy 审批。

6. **Agent 授权**：按产品现有规则决定哪些 Agent 能使用该工具；能力包不读产品账号或工作区。

## Tron 集成点（lead 后续）

1. `tron-backend/Cargo.toml` 增加 path 依赖
   `eliteforge-agent-capability-remote-access = { path = "crates/remote-access-capability" }`。
2. 新增宿主适配器，建议放在 `tron-backend/src/application/agent/framework_runtime/host_remote_access.rs`。
   它复用 `AppState.remote_access`、`AppState::remote_access_active()`、`AppConfig::from_yaml_file`
   和 `system_config.update`；`status`/`pairing`/`rotate_token`/`set_enabled` 的共享逻辑建议从
   `tron-backend/src/adapters/inbound/http/remote_access.rs` 提取到 application 层，HTTP 处理器只保留
   入站 DTO、`LocalOnly` 校验和错误映射。`rotate_token` 必须持久化新令牌并更新运行时；
   `set_enabled` 返回的快照应包含 `restartRequired`，监听器是否需要重启由宿主判断。
3. `tron-backend/src/application/agent/framework_runtime/host_environment.rs`：在
   `FrameworkBuilder` 链上增加 `.capability_pack(RemoteAccessCapability::new(...).capability_pack())`；
   同时在 `reconcile_capability_resources` 增加
   `sources.push(("framework.remote-access".into(), registry_skill_sources(&pack)))`，让技能资源随能力包
   安装/卸载同步。
4. `tron-backend/src/application/agent/framework_runtime/capability_catalog.rs`：
   `capability_pack_entries` 增加 `framework.remote-access` 条目（名称、描述、provider `Tron`、版本）；
   `pack_extension_implementations` 增加该工具实现，便于能力中心和扩展页展示。
5. `tron-backend/crates/tron-harness-capability/src/tool_risk.rs`：为
   `framework.remote-access.manage` 增加风险映射。未知工具默认按 `operation_kind = Command` 落到
   Medium；令牌轮换和启停建议显式归为 High，并在工具权限页展示。
6. 验证：本 crate 独立 workspace，运行 `cargo check` 即可；如需并入统一入口，可给根 Makefile
   增加对应 target（本 crate 不回改 Makefile）。

## 构建验证

在 crate 目录执行（独立 workspace，不构建 Tron 主工程）：

```bash
cd crates/capability/eliteforge-agent-capability-remote-access
cargo check
```

结果：`Finished dev profile [unoptimized + debuginfo] target(s)`，退出码 0。Framework 依赖按
`tag = "1.0.0"` 解析到 `e76bef82`，与 `tron-backend/Cargo.lock` 固定的提交一致；未运行主工程构建，
未编写测试代码。接入到 `tron-backend` 后，主工程构建校验由 lead 负责。