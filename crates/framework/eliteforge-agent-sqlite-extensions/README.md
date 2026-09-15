# eliteforge-agent-sqlite-extensions

从 Tron 的 `shared/eliteforge_agent_sqlite_extensions.rs` 提取的 SQLite 原生扩展加载包。
包内不依赖 EliteForge Agent Framework、Tron 配置、环境变量或业务表。

SQLite 本体通过 rusqlite `bundled` 编译，平台构建由 `libsqlite3-sys` 和目标 C 工具链处理。
本包负责的是 sqlite-simple v0.7.1 与 sqlite-vec v0.1.9 的原生扩展、字典、平台选择和连接级加载。

- 默认不嵌入或加载原生资源。`bundled-native` feature 只将编译目标对应的动态库及字典嵌入程序。
- 调用方指定受保护的缓存目录；程序从嵌入的字节生成真实文件，不访问编译机器的源码目录。
- 每个物理连接都要调用 `prepare_connection`。加载结束后关闭 SQLite 的扩展加载开关。
- `Disabled` 不加载，`Preferred` 返回可查询的不可用原因，`Required` 在失败时返回错误。
- 缓存按内容 SHA-256 分目录；并发发布使用临时文件和不覆盖的原子发布，已存在文件检查完整内容。
- 平台选择区分 OS、CPU、环境和 iOS simulator ABI。没有对应资源时明确不可用；Linux musl 不使用 glibc 扩展。
- 资源目录包含上游原生库与许可证，实际编译只包含匹配目标的资源。保留文件不代表已在该平台运行验证。

```toml
eliteforge-agent-sqlite-extensions = { version = "0.1.0", features = ["bundled-native"] }
```

```rust
use eliteforge_agent_sqlite_extensions::{ExtensionConfig, ExtensionMode};

let mut extensions = ExtensionConfig::new("private-data/sqlite-native");
extensions.simple = ExtensionMode::Required;
extensions.vector = ExtensionMode::Preferred;
let report = extensions.prepare_connection(&connection)?;
```

缓存目录应由应用管理，不能允许不可信进程修改。这里装载的是经过包内资源校验的原生代码，
不是接收用户路径的任意动态库加载接口。

当前工作区使用 rusqlite 0.37 / libsqlite3-sys 0.35。Tron 原有 rusqlite 0.32 在接入时需要统一版本，
不能把两个不同版本的 SQLite native link 包同时链接进一个 Rust 程序。

原生资源来源：
[sqlite-simple](https://github.com/wangfenjin/simple/releases/tag/v0.7.1)、
[sqlite-vec](https://github.com/asg017/sqlite-vec/releases/tag/v0.1.9)。
