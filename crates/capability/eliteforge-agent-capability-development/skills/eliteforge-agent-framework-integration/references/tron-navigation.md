# Tron 能力包导航契约

仅适用于 Tron 宿主的 `tron/navigation.json`。规则依据 Tron 的 `src/domain/navigation/model.rs`、`src/application/navigation/packs.rs`、`src/application/navigation/tree.rs`、`src/application/agent/framework_runtime/host_navigation.rs` 及 `host_pages.rs`（均相对于 `tron-backend/`）。针对其他 Tron 版本，核对对应版本实现。

## 编码与引用

| 字段 | 必须满足 |
| --- | --- |
| `domains[].code` | 原样匹配 `^[A-Z0-9_]{1,64}$`，领域内无重名 |
| `menus[].code` | 原样匹配 `^[A-Z0-9_]{1,64}$`，菜单内无重名 |
| `layouts` 的对象键 | 与本包某个 `domains[].code` 完全相同 |
| `layouts.*[].menuCode` | 与本包某个 `menus[].code` 完全相同，在同一布局内只出现一次 |
| `layouts.*[].parentMenuCode` | 根节点用 `null` 或省略；其他节点精确引用同一布局中的 `Group` 菜单，不能自引用或成环 |
| `menus[].pageCode`（`PackPage`） | 与本包 `tron/pages.json` 中某个 `pages[].id` 完全相同；由 Tron 在读取时加包 ID 前缀 |

领域编码与菜单编码分别做全局重名检查。同一包升级可继续使用自己已有的编码，不能占用已有内置、用户自建或其他包的同类编码。选用稳定的包专属前缀是生成约定，后端并不要求某个固定前缀；发现冲突时重新选取编码，并同步所有引用。若修复已发布的错误编码，核对已有布局与升级结果，不将重命名当作无影响的格式化。

合法菜单编码示例：`EXAMPLE_PAGE_DEMO_OVERVIEW`。以下都不合法：`example.page-demo.overview`、`example_overview`、`EXAMPLE-OVERVIEW`、` EXAMPLE_OVERVIEW `、空串及超过 64 个字符的编码。正则不要求首字符必须是字母；不要额外宣称后端有这个限制。

底层通用编码函数会 trim 并转 ASCII 大写，但能力包校验还会要求结果与输入原文完全一致，因此带空白或小写的声明会被拒绝，而不是自动修正。

```text
包 ID：example.page-demo
  ├─ 领域编码：EXAMPLE_PAGE_DEMO
  ├─ 菜单编码：EXAMPLE_PAGE_DEMO_OVERVIEW
  └─ 页面 ID：overview ← PackPage.pageCode 原样引用
```

页面 ID 的规则不同：`^[A-Za-z0-9_-]{1,64}$`，在本包内唯一。`Internal.pageCode` 则引用 Tron 已有页面，例如 `agents.index`；不能把菜单编码正则套到这些目标字段上。

## 完整导航示例

以下是完整的 `tron/navigation.json`，假设本包已在 `tron/pages.json` 声明页面 `overview`：

```json
{
  "version": 1,
  "domains": [
    {
      "code": "EXAMPLE_PAGE_DEMO",
      "name": {"zh-CN": "页面示例", "en-US": "Page demo"}
    }
  ],
  "menus": [
    {
      "code": "EXAMPLE_PAGE_DEMO_GROUP",
      "name": {"zh-CN": "示例入口", "en-US": "Demo pages"},
      "menuType": "Group",
      "pageCode": null,
      "url": null
    },
    {
      "code": "EXAMPLE_PAGE_DEMO_OVERVIEW",
      "name": {"zh-CN": "概览", "en-US": "Overview"},
      "menuType": "PackPage",
      "pageCode": "overview",
      "url": null
    }
  ],
  "layouts": {
    "EXAMPLE_PAGE_DEMO": [
      {"menuCode": "EXAMPLE_PAGE_DEMO_GROUP", "parentMenuCode": null, "position": 0, "childDisplay": "Inline"},
      {"menuCode": "EXAMPLE_PAGE_DEMO_OVERVIEW", "parentMenuCode": "EXAMPLE_PAGE_DEMO_GROUP", "position": 0, "childDisplay": "Inline"}
    ]
  }
}
```

声明文件使用 camelCase 字段；未知字段会被拒绝。`version` 必须为 `1`，领域和菜单的 `name` 必须有非空 `zh-CN`、`en-US`。`position` 为非负整数，同一父节点下不能重复；层级不能超过目标宿主配置的深度限制。`Group` 不填写页面或 URL；`PackPage` 必须有有效 `pageCode`，不能同时填写 URL。

## 打包与发布前检查

1. 将 `tron/navigation.json` 写入 manifest 的 `resources`；使用包页面时也声明 `tron/pages.json` 及所需页面资源。未列入 `resources` 的导航文件不会被 Tron 读取。
2. 检查最终 ZIP 内的声明，而不只检查工作目录：每个编码原样符合正则，领域和菜单各自无重复，布局及父子引用完整，页面 ID 存在，资源文件齐全。
3. 通过目标 Tron 的导航校验确认编码与既有资源不冲突。Framework 的签名、归档和清单检查不代替 `NavigationContribution::validate` 与 `NavigationService::validate_pack` 的宿主检查；不要编造不存在的 CLI 校验命令。
4. 修复同一能力包的新版本时，保持 manifest 包 ID 不变，提升语义版本，重新打包并更新相应哈希/签名。ZIP 文件名改版本号不等于 manifest 已更新；不要覆盖已发布版本来掩盖修复。
5. 按用户授权执行真实导入或升级，启用后确认菜单可见且能打开目标页，再报告验收结果。仅完成静态检查时如实报告；本技能不自动授权上传或发布。
