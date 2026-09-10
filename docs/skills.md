# Skills

框架提供的默认 Skills 插件遵循 [Agent Skills](https://agentskills.io/specification) 的渐进式加载方式。它只负责发现、选择、上下文提示和按需读取；具体的技能内容、权限和业务流程仍由上层应用决定。

官方插件包默认启用 Skills。上层应用也可以单独注册 SkillsPlugin，或在官方包中替换、关闭它。

## 默认目录

默认 Provider 在创建配置时记录两个目录：

~~~text
~/.agents/skills/<skill-name>/SKILL.md
<当前工作目录>/.agents/skills/<skill-name>/SKILL.md
~~~

Windows 使用 %USERPROFILE%/.agents/skills。目录不存在时视为空。两个目录存在同名技能时，本地目录覆盖系统级（当前用户范围）目录；框架不向父目录查找。

每个技能至少包含 SKILL.md。文件以 YAML frontmatter 开始，必须有 name 和 description；name 必须与目录名相同，最多 64 个 Unicode 字符，只能使用小写字母、数字和连字符。scripts、references、assets 及其他普通文件可以作为按需资源。

~~~text
release-notes/
├── SKILL.md
├── references/
│   └── style.md
└── scripts/
    └── publish.sh
~~~

## 能力包来源

能力包可以把 Skills 当作包成员。选中包后，框架把包资源、目录来源和声明式 Provider 汇聚到一个
`SkillsPlugin` 运行时，因此不会重复注册上下文、`skill_load` 或 `skill_read_resource` 工具。

发布能力包时，将资源放到 crate 的默认 `skills/` 目录：

~~~text
shop-capabilities/
├── src/
└── skills/
    └── order-review/
        ├── SKILL.md
        ├── scripts/
        ├── references/
        └── assets/
~~~

在这个 crate 内声明包：

~~~rust
use eliteforge_agent_core::{CapabilityPackBuilder, capability_skills};

pub fn shop_pack(config: ShopConfig) -> CapabilityPackBuilder<ShopConfig> {
    CapabilityPackBuilder::new("shop", config)
        .skills(capability_skills!())
}
~~~

为保证新增资源也会被编入下一次构建，crate 还需要一个 `build.rs`：

~~~rust
use eliteforge_agent_core::capability_skills_build;

fn main() {
    capability_skills_build!();
}
~~~

在 `Cargo.toml` 的 `build-dependencies` 中加入 `eliteforge-agent-core`。若资源目录写成
`capability_skills!("package-skills")`，build script 对应使用
`capability_skills_build!("package-skills")`。

`capability_skills!()` 会将目录内每个普通文件编入产物，并为内容生成 SHA-256 摘要。应用启用这类包
时，必须指定可共享的资源根：

~~~rust
let agent = AgentBuilder::new()
    .capability_skills_root(app_data.join("agent-resources"))
    .capability_pack(shop_pack(config))
    .build()
    .await?;
~~~

资源准备到
`<资源根>/capability-packs/<安全包ID>/<内容摘要>/skills/`。框架使用临时目录和原子发布；同一内容
复用，不同版本并存，关闭 Agent 不会删除共享缓存。开发期若由应用直接管理目录，可改用
`.skills_directory("./company-skills")`，不会复制资源也不需要资源根。

非文件系统来源直接实现并声明 `SkillProvider`：

~~~rust
#[capability(pack = "shop")]
#[async_trait::async_trait]
impl SkillProvider for ShopSkillProvider {
    // catalog / load / read_resource / release
}
~~~

框架会自动创建聚合 Skills 运行时。若应用已注册 `SkillsPlugin`，它会在 `ready` 阶段绑定所有来源；
`only_skills(...)` 对合并后的目录生效。不同来源同名技能会在该逻辑运行建立清单时报告冲突，
不会任意选一个。单个文件系统 Provider 内，仍由本地目录覆盖系统目录。

## 运行过程

~~~mermaid
flowchart LR
    A[系统级目录（当前用户）] --> C[发现 name 与 description]
    B[当前目录本地技能] --> C
    C --> D[上层应用选择生效技能]
    D --> E[模型上下文中的技能清单]
    E --> F[skill_load]
    F --> G[完整 SKILL.md 与资源清单]
    G --> H[skill_read_resource]
~~~

发现阶段只读取 frontmatter，不读取 SKILL.md 正文或资源内容。模型上下文包含名称、描述、来源和技能目录；它必须先调用 skill_load，再遵循该技能的指令。skill_load 返回完整 SKILL.md 和文件列表。只有该调用成功后，skill_read_resource 才能读取一个列出的 UTF-8 文本资源。

脚本不会因为发现或加载而执行。需要运行脚本时，模型使用已经显式装配的系统命令工具；这仍会经过 ToolPolicy、审批和上层应用的隔离策略。

每个逻辑运行都有独立的技能快照：同一次运行中已读取的正文和资源保持缓存一致。上下文与工具并发请求同一运行的清单时，也只会发现一次。暂停后在同一 Agent 内恢复会继续使用原快照；正文或资源在发现后、首次读取前发生变化时会返回错误，要求发起新的运行。进程重建后的恢复会重新发现目录；框架只会根据持久化的成功 `skill_load` 记录，为挂起的 `skill_read_resource` 重新验证其技能资源清单，不承诺跨进程保存旧文件快照。

## 配置与选择

直接依赖官方 Skills crate 时：

~~~toml
[dependencies]
eliteforge-agent-official-plugin-skills = "0.1.0"
~~~

下面的配置只扫描指定目录，并仅让 release-notes 生效：

~~~rust
use eliteforge_agent_framework::AgentBuilder;
use eliteforge_agent_official_plugin_skills::{
    FileSystemSkillsConfig, SkillsPlugin,
};

let skills = SkillsPlugin::filesystem(
    FileSystemSkillsConfig::new()
        .with_system_directory("/opt/company-skills")
        .with_local_directory("./.agents/skills")
        .with_max_skill_markdown_bytes(256 * 1024)
        .with_max_resource_bytes(512 * 1024),
)
.only_skills(["release-notes"]);

let agent = AgentBuilder::new()
    .plugin(skills)
    // 其余模型、存储和循环配置
    .build()
    .await?;
~~~

only_skills 传入空集合时不暴露任何技能。指定的技能在本次执行发现不到时，会在模型调用前返回配置错误。all_skills 可以恢复默认的全部发现结果。

使用官方插件包时，可以替换或关闭默认实现：

~~~rust
use eliteforge_agent_official_plugin_pack::EliteForgeOfficialPluginPack;
use eliteforge_agent_official_plugin_skills::{FileSystemSkillsConfig, SkillsPlugin};

let pack = EliteForgeOfficialPluginPack::new("agent.sqlite")
    .skills(SkillsPlugin::filesystem(
        FileSystemSkillsConfig::new()
            .with_local_directory("./.agents/skills"),
    ));

// 不需要 Skills 的 Agent：
let without_skills = EliteForgeOfficialPluginPack::new("agent.sqlite")
    .without_skills();
~~~

## 自定义 Provider

SkillProvider 是 Core 的存储无关接口。Tron 后续可以用它接入已有的 RAG 技能索引，而不把平台、空间或 RAG 依赖带入框架默认实现：

~~~rust
use async_trait::async_trait;
use eliteforge_agent_core::{
    AgentError, RunContext, SkillCatalog, SkillDetail, SkillProvider, SkillResource,
};
use eliteforge_agent_official_plugin_skills::SkillsPlugin;

struct RagSkillProvider;

#[async_trait]
impl SkillProvider for RagSkillProvider {
    async fn catalog(&self, context: &RunContext) -> Result<SkillCatalog, AgentError> {
        // 返回当前执行的轻量技能元数据
        todo!()
    }

    async fn load(
        &self,
        context: &RunContext,
        name: &str,
    ) -> Result<SkillDetail, AgentError> {
        // 按名称加载完整技能正文和资源索引
        todo!()
    }

    async fn read_resource(
        &self,
        context: &RunContext,
        name: &str,
        path: &str,
    ) -> Result<SkillResource, AgentError> {
        // 按需读取一个资源
        todo!()
    }
}

let plugin = SkillsPlugin::new(RagSkillProvider);
~~~

Provider 应为同一逻辑 `run_id` 保持一致的目录，并在 release 中清理运行级缓存。框架会负责技能选择、上下文清单、模型工具和逻辑运行结束时调用 release。

## 边界与诊断

SkillsPlugin 只会把 `skill_load` 返回清单中的正常相对路径交给 Provider；文件系统 Provider 还会拒绝符号链接逃逸。它限制技能总数、frontmatter、SKILL.md 正文、资源大小、资源数量和资源深度；超限时明确报错，不静默截断正文或资源。标准 frontmatter 的 `allowed-tools` 会随完整 `SKILL.md` 返回，但不会绕过 `ToolPolicy` 或审批。

格式错误、无效 YAML、名称不匹配和被忽略的符号链接不会阻断其他有效技能。它们记录在 SkillCatalog 的 diagnostics 中；上层应用可调用 FileSystemSkillProvider::discover 查询，也可以在有诊断时让模型调用 skill_diagnostics。
