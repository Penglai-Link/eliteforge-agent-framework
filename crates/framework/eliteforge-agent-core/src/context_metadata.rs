use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Source evolution between requests. Each request is still assembled afresh;
/// this declaration does not mutate conversation storage or change assembly policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextBlockUpdateMode {
    /// Instruction content stays fixed within one run.
    Fixed,
    /// New entries extend the source; budget reduction may still shorten it.
    Append,
    /// A new snapshot supersedes this block's previous snapshot.
    Replace,
    /// The contributor has not declared how its source evolves.
    #[default]
    Unspecified,
}

/// Display metadata for a context contributor in host layout controls.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextBlockMetadata {
    pub display_name: String,
    pub description: String,
    pub update_mode: ContextBlockUpdateMode,
    /// Provider-owned reduction semantics. Hosts must not infer these from block IDs.
    pub compression: ContextCompressionMetadata,
    /// Contributor-owned translations, keyed by BCP 47 locale (for example zh-CN).
    pub locales: BTreeMap<String, ContextBlockText>,
    /// Relative default ordering for newly discovered blocks. Existing user order wins.
    pub default_order: i32,
}

/// Localized explanation of a context block's contents, independent of its budget.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextBlockText {
    pub display_name: String,
    pub description: String,
}

impl ContextBlockMetadata {
    pub fn with_compression(mut self, compression: ContextCompressionMetadata) -> Self {
        self.compression = compression;
        self
    }

    pub fn with_update_mode(mut self, update_mode: ContextBlockUpdateMode) -> Self {
        self.update_mode = update_mode;
        self
    }

    pub fn new(display_name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            display_name: display_name.into(),
            description: description.into(),
            ..Self::default()
        }
    }

    pub fn with_locale(
        mut self,
        locale: impl Into<String>,
        display_name: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        self.locales.insert(
            locale.into(),
            ContextBlockText {
                display_name: display_name.into(),
                description: description.into(),
            },
        );
        self
    }

    /// Resolve one host-selected locale: exact match, language match, then base text.
    /// Missing translated fields fall back independently to the contributor's base text.
    pub fn resolve(&self, locale: &str) -> ContextBlockText {
        resolve_text(&self.display_name, &self.description, &self.locales, locale)
    }
}

/// Extensible, localized strategy description owned by the source or reducer.
/// An empty ID means undeclared; custom IDs require no host-side enum or mapping.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextCompressionMetadata {
    pub id: String,
    pub display_name: String,
    pub description: String,
    #[serde(default)]
    pub locales: BTreeMap<String, ContextBlockText>,
}

impl ContextCompressionMetadata {
    pub fn new(
        id: impl Into<String>,
        display_name: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            display_name: display_name.into(),
            description: description.into(),
            locales: BTreeMap::new(),
        }
    }

    pub fn with_locale(
        mut self,
        locale: impl Into<String>,
        display_name: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        self.locales.insert(
            locale.into(),
            ContextBlockText {
                display_name: display_name.into(),
                description: description.into(),
            },
        );
        self
    }

    pub fn resolve(&self, locale: &str) -> ContextBlockText {
        resolve_text(&self.display_name, &self.description, &self.locales, locale)
    }

    pub fn keep() -> Self {
        Self::new("keep", "Keep unchanged", "Preserve the complete block without calling a model. Exceeding its budget reports an error.")
            .with_locale("zh-CN", "原样保留", "完整保留内容，不调用模型压缩；超出预算时报告错误。")
    }

    pub fn bound() -> Self {
        Self::new(
            "bound",
            "Bound to budget",
            "Trim or select content to fit the budget without calling a model.",
        )
        .with_locale(
            "zh-CN",
            "按预算裁剪",
            "通过裁剪或筛选内容满足预算，不调用模型。",
        )
    }

    pub fn semantic() -> Self {
        Self::new("semantic", "Semantic summary", "Use a model to summarize reference data within the budget, retaining relevant information.")
            .with_locale("zh-CN", "语义摘要", "调用模型将参考数据概括到预算范围内，保留相关信息。")
    }

    pub fn history() -> Self {
        Self::new("checkpoint", "History summary", "Use a model to summarize earlier conversation turns while retaining recent complete turns and tool-call boundaries. Compression follows the configured triggers and budget limits.")
            .with_locale("zh-CN", "历史摘要", "调用模型概括较早对话，保留近期完整轮次及工具调用边界；按配置的触发条件和预算限制执行压缩。")
    }

    pub fn coordinated_summary() -> Self {
        Self::new("coordinated-summary", "Updated with history", "The history compressor writes this summary within its own budget. It is not independently truncated.")
            .with_locale("zh-CN", "随历史摘要更新", "由历史压缩流程按本块预算生成摘要，不再独立裁剪。")
    }
}

fn resolve_text(
    display_name: &str,
    description: &str,
    locales: &BTreeMap<String, ContextBlockText>,
    locale: &str,
) -> ContextBlockText {
    let locale = locale.trim();
    let language = locale.split('-').next().unwrap_or(locale);
    let text = locales
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(locale))
        .or_else(|| {
            locales
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(language))
        })
        .or_else(|| {
            locales.iter().find(|(key, _)| {
                key.split('-')
                    .next()
                    .is_some_and(|part| part.eq_ignore_ascii_case(language))
            })
        })
        .map(|(_, text)| text);
    ContextBlockText {
        display_name: text
            .map(|text| text.display_name.as_str())
            .filter(|text| !text.trim().is_empty())
            .unwrap_or(display_name)
            .to_owned(),
        description: text
            .map(|text| text.description.as_str())
            .filter(|text| !text.trim().is_empty())
            .unwrap_or(description)
            .to_owned(),
    }
}
