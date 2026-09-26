# 动态能力包迁移

这次重构合并能力包组合与插件组合，只保留 `CapabilityPack`。

| 原 API | 当前 API |
| --- | --- |
| `PluginPack` / `PluginPackBuilder` | `CapabilityPack` / `CapabilityPackBuilder` |
| `AgentBuilder::plugin_pack(pack)` | `AgentBuilder::capability_pack(pack)` |
| `EliteForgeOfficialPluginPack` | `EliteForgeOfficialCapabilityPack` |
| `eliteforge-agent-official-plugin-pack` | `eliteforge-agent-official-capability-pack` |
| `into_plugins()` 组合展开 | `plugins()` 和 `included_packs()` 分别声明直接成员与子包 |

`.plugin(...)` 保留直接装配。要参与动态重配，使用 `ConfiguredPluginFactory`；已有直接实例只保留原配置。工厂负责创建新配置实例，install/ready 仍由内核执行，不在工厂中获取运行资源。

```rust,ignore
let pack = CapabilityPackBuilder::new("application", configuration)
    .include(official_pack)
    .plugin_factory(ConfiguredPluginFactory::new("custom.plugin", vec![], || {
        Ok(std::sync::Arc::new(CustomPlugin::new()))
    }));
let agent = AgentBuilder::new().capability_pack(pack).build().await?;
```

`TokenEstimator::message/tools/options/request` 改为异步并返回 `Result<usize, ModelError>`；调用链要加 `.await?`。`ContextBudgetPolicy::allocate` 同样改为异步、可失败。同步元数据方法仍不进行远程 I/O；模型选择、AgentLoop、工作流节点和执行器在装配阶段增加异步校验钩子。

Checkpoint 增加可缺省的 `capability_versions` 字段，旧静态任务可继续读取；由外部能力包创建的等待任务必须用原版本与摘要恢复。框架不会把旧任务静默升级到新实现。

`Agent` 现在是稳定装配入口。它的克隆、任务控制和等待结果仍有效；`.reconfigure(builder)` 切换后，克隆使用新装配。保留未消费的 RunHandle、实时会话、远程资源或页面时，先释放它们再重配。

自定义跨进程扩展要声明稳定契约 ID、版本和数据结构，使用实现目录调用；原生 `Any` / `TypeId` / 闭包仅用于宿主内扩展。独立输出通过 `RemoteOutputs` 获取，具体强类型宿主输出仍可以注册原生适配器。

页面使用通用 TypeScript SDK，由宿主提供挂载容器和桥接实现。包内页面不注入官方页面，也不自行取得宿主能力。插件内部 OTel 配置独立于 Framework 的观测插件。

## Context 存储契约

`ArtifactStore`／`SummaryStore` 及相关类型拆到 `eliteforge-agent-context-contracts`；原 `eliteforge-agent-context` 的导出仍保留。插件只依赖契约时可改用轻量 crate，不会带入 SQLite 或文件实现。远程清单使用 `context.artifact-store`／`context.summary-store`，SDK 使用对应 `TypedBackend` 注册方法。

外部视频任务 ID 现在是实例绑定的宿主句柄，必须保存完整 `VideoJob` 并通过原 Agent 查询／取消。未结束的任务阻止重配；更新后旧句柄明确失败。原生静态模型的任务 ID 行为保留。

## 结果未知的错误

模型、Agent 和各类存储错误增加 `Indeterminate(String)` 分支。超时、断连或后端调用中途崩溃可能已经产生效果，不等同于明确失败，也不会进入 transient 重试／模型切换。穷举匹配错误的宿主需要补上该分支；持久化状态未知时保留版本保护，核对后再恢复。
