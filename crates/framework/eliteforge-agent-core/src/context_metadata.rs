use std::collections::BTreeMap;

/// Display metadata for a context contributor in host layout controls.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextBlockMetadata {
    pub display_name: String,
    pub description: String,
    /// Contributor-owned translations, keyed by BCP 47 locale (for example zh-CN).
    pub locales: BTreeMap<String, ContextBlockText>,
    /// Relative default ordering for newly discovered blocks. Existing user order wins.
    pub default_order: i32,
}

/// Localized explanation of a context block's contents, independent of its budget.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextBlockText {
    pub display_name: String,
    pub description: String,
}

impl ContextBlockMetadata {
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
        let locale = locale.trim();
        let language = locale.split('-').next().unwrap_or(locale);
        let text = self
            .locales
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(locale))
            .or_else(|| {
                self.locales
                    .iter()
                    .find(|(key, _)| key.eq_ignore_ascii_case(language))
            })
            .or_else(|| {
                self.locales.iter().find(|(key, _)| {
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
                .unwrap_or(&self.display_name)
                .to_owned(),
            description: text
                .map(|text| text.description.as_str())
                .filter(|text| !text.trim().is_empty())
                .unwrap_or(&self.description)
                .to_owned(),
        }
    }
}
