# Plugin、Middleware 与 Hook

Plugin 是安装入口。Middleware 负责拦截执行，Hook 负责接收生命周期通知。
三个概念分别对应装配、执行控制和生命周期，注册类型与调用入口独立。
第一次开发插件先看[插件开发指南](plugins.md)；全部能力契约与注册方式见[扩展点参考](plugin-extension-reference.md)，分层与默认装配见[架构总览](architecture.md)。

```text
Plugin.install
├─ contribute(RunMiddlewareService / ToolMiddlewareService / ...)
│                     └─ 执行入口调用 intercept(..., next)
└─ contribute(RunLifecycleHookService)
                      └─ 生命周期入口调用 on_started / on_finished
```

## 执行中间件

Run、工作流节点、模型、工具均使用 `intercept + next`，先贡献的中间件在外层。
顺序是服务贡献顺序；插件依赖会影响实际安装顺序。

```text
RunLifecycleHook.on_started
    │
    A.intercept 前置
      B.intercept 前置
        真正执行 + Framework 资源清理
      B.intercept 后置
    A.intercept 后置
    │
RunLifecycleHook.on_finished（读取最终结果）
```

Run 示例：

```rust,ignore
#[async_trait]
impl RunMiddleware for MyInterceptor {
    async fn intercept(
        &self,
        request: &RunRequest,
        context: &RunContext,
        next: RunNext<'_>,
    ) -> Result<RunResult, AgentError> {
        // 不调用 next、直接返回 Err，可拒绝执行。
        let result = next.run().await;
        // 需要异步收尾时，用此函数保留执行和清理的两个错误。
        finish_with_cleanup(result, self.cleanup().await)
    }
}
```

| 类型 | continuation | 输入边界 |
|---|---|---|
| `RunMiddleware` | `RunNext::run()` | 绑定已经准入的请求、身份、限额和上下文 |
| `WorkflowNodeMiddleware` | `WorkflowNodeNext::run(input, context)` | 节点身份和返回路由经过校验 |
| `ModelMiddleware` | `ModelNext::run(request, context)` | 允许处理请求，不能改变模型目标或引入未授权工具 |
| `ToolMiddleware` | `ToolNext::run()` | 绑定已经选定的工具及参数；派发前重查撤销、取消和 deadline |

next 按值消费，一条 continuation 最多执行一次。Run 和工具的已准入输入不能通过传参替换。
工具权限策略仍在派发前生效，中间件不会授予额外权限。

执行错误处理遵循各操作的语义。Run 拦截器可返回替代结果，清理失败应使用
`finish_with_cleanup` 保留主错误。工具执行错误或内层拒绝不能被外层拦截器改成成功；
工具已经成功派发后，中间件再报错、panic 或丢失结果时标记 `Indeterminate`，避免自动重复副作用。

Framework 管理取消和超时。每层拦截器可以在内层中断后执行有界收尾；Run 使用
`cleanup_timeout`，工具使用该次调用的 timeout 作为收尾上限。超出期限的 future 会被丢弃，
不能保证 await 后的代码一定运行。同步兜底使用 RAII，资源的异步关闭交给 Framework 管理。

## 生命周期 Hook

当前提供 `RunLifecycleHook`：

```rust,ignore
#[async_trait]
impl RunLifecycleHook for MyLifecycleHook {
    async fn on_started(&self, context: &RunLifecycleContext<'_>) {
        // 接收逻辑 run ID、本次 execution ID 和 session ID。
    }

    async fn on_finished(
        &self,
        context: &RunLifecycleContext<'_>,
        result: &Result<RunResult, AgentError>,
    ) {
        // 接收包含中间件处理及资源清理在内的最终结果。
    }
}
```

Hook 没有 next，返回值是 `()`，不能通过返回错误否决运行或修改结果。
参数只包含只读身份及结果，不提供可取消、撤销工具的 RunContext。
这是一种接口职责约束；可信进程内插件仍可能持有自己的外部资源，不构成安全沙箱。

每次准入执行（包括恢复）都会尝试开始和结束通知，中间件拒绝执行也会触发结束通知。
两种通知均按贡献顺序调用；它们不是嵌套资源获取/释放，因此结束通知不倒序执行。
开始通知在取消或 deadline 到达后可跳过；结束通知仍尝试执行，每个回调最多等待
`cleanup_timeout`。panic、超时和开始通知中断通过 `LifecycleHookFailed` 事件报告，带阶段、
贡献序号及分类，不改变运行结果，也不阻止其他 Hook 的通知。

Hook 在运行任务内直接调用，与有队列的 EventSink 分开。需要异步 UI 更新、分析订阅等
低耦合通知时使用 EventSink；需要生命周期时点通知时使用 Hook；需要控制执行时使用 Middleware。
Hook 不承担资源关闭保证。硬进程退出或运行时被销毁不能保证收到结束通知。

插件的安装、回滚、关闭及 Drop 兜底仍归 Plugin Kernel 管理：`Plugin::install`、
`PluginContext::on_shutdown` 和 `on_cleanup`。新增生命周期类型时应继续使用明确命名的 Hook
契约，不把执行前后逻辑重新塞进 Hook。

## 插件可以同时贡献两者

```rust,ignore
context.contribute(RunMiddlewareService(interceptor))?;
context.contribute(RunLifecycleHookService(lifecycle_hook))?;
```

也可以分别使用 `RunMiddlewarePlugin::new(...)`、`RunLifecycleHookPlugin::new(...)`。
贡献使用 `contribute(T)`，同一类型允许多个实现。

这次 API 调整移除了 Run/Tool Middleware 的 `before/after` 方法。迁移前置逻辑到
`next.run().await` 之前，结果处理迁移到它之后；只有生命周期通知才迁移到 Hook。
`TelemetryPlugin` 和 `ToolOutputOffloader` 都已迁移为执行拦截器。
