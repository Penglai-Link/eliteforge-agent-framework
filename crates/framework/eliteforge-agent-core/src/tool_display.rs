//! Plugin-owned, human-facing tool resources. Resolution never mutates the
//! descriptor used for model calls, validation, admission or execution.
use crate::ToolDescriptor;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDisplayText {
    pub name: Option<String>,
    pub description: Option<String>,
    /// JSON pointers to schema nodes, e.g. /properties/path. Only description
    /// text is replaced; types, constraints and property names stay unchanged.
    #[serde(default)]
    pub parameters: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDisplayMetadata {
    pub default_locale: String,
    pub locales: BTreeMap<String, ToolDisplayText>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedToolDisplay {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

impl ToolDisplayMetadata {
    fn texts(&self, accept_language: &str) -> Vec<&ToolDisplayText> {
        let mut preferences = accept_language
            .split(',')
            .enumerate()
            .filter_map(|(index, item)| {
                let mut parts = item.trim().split(';');
                let locale = parts.next()?.trim();
                let quality = parts
                    .find_map(|p| p.trim().strip_prefix("q="))
                    .map_or(Some(1.0), |q| q.parse::<f32>().ok())?;
                (quality > 0.0 && quality <= 1.0).then_some((locale, quality, index))
            })
            .collect::<Vec<_>>();
        preferences.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.2.cmp(&b.2)));
        let mut result = Vec::new();
        for (locale, _, _) in preferences {
            if let Some((_, text)) = self
                .locales
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(locale))
            {
                result.push(text);
            } else {
                let language = locale.split('-').next().unwrap_or(locale);
                if let Some((_, text)) = self.locales.iter().find(|(key, _)| {
                    key.split('-')
                        .next()
                        .is_some_and(|l| l.eq_ignore_ascii_case(language))
                }) {
                    result.push(text);
                }
            }
        }
        if let Some(text) = self.locales.get(&self.default_locale) {
            result.push(text);
        }
        result
    }
    pub fn resolve(
        &self,
        locale: &str,
        name: &str,
        description: &str,
        schema: &Value,
    ) -> ResolvedToolDisplay {
        let texts = self.texts(locale);
        let mut input_schema = schema.clone();
        // Lower-priority resources first, so missing fields fall back separately.
        for text in texts.iter().rev() {
            for (pointer, description) in &text.parameters {
                if let Some(node) = input_schema
                    .pointer_mut(pointer)
                    .and_then(Value::as_object_mut)
                {
                    node.insert("description".into(), description.clone().into());
                }
            }
        }
        ResolvedToolDisplay {
            name: texts
                .iter()
                .find_map(|t| t.name.as_ref())
                .cloned()
                .unwrap_or_else(|| name.into()),
            description: texts
                .iter()
                .find_map(|t| t.description.as_ref())
                .cloned()
                .unwrap_or_else(|| description.into()),
            input_schema,
        }
    }
}

impl ToolDescriptor {
    pub fn with_display(mut self, display: ToolDisplayMetadata) -> Self {
        self.display = Some(display);
        self
    }
    /// Attach a bundled plugin resource keyed by stable tool ID. Malformed
    /// resources are programming errors; remote metadata uses normal serde validation.
    pub fn with_display_resource(mut self, resource: &'static str) -> Self {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Resource {
            default_locale: String,
            tools: BTreeMap<String, BTreeMap<String, ToolDisplayText>>,
            #[serde(default)]
            parameters: BTreeMap<String, BTreeMap<String, String>>,
        }
        // Only immutable bundled resources are cached. Localized results are
        // always resolved per request and are never shared between languages.
        type Resources = std::collections::HashMap<&'static str, std::sync::Arc<Resource>>;
        static RESOURCES: std::sync::OnceLock<std::sync::RwLock<Resources>> =
            std::sync::OnceLock::new();
        let cache = RESOURCES.get_or_init(Default::default);
        let cached = cache
            .read()
            .expect("tool display resources")
            .get(resource)
            .cloned();
        let resource = if let Some(cached) = cached {
            cached
        } else {
            cache
                .write()
                .expect("tool display resources")
                .entry(resource)
                .or_insert_with(|| {
                    std::sync::Arc::new(
                        serde_json::from_str::<Resource>(resource)
                            .expect("invalid bundled tool display resource"),
                    )
                })
                .clone()
        };
        if let Some(locales) = resource.tools.get(&self.id) {
            let mut locales = locales.clone();
            for (locale, parameters) in &resource.parameters {
                let text = locales.entry(locale.clone()).or_default();
                for (pointer, description) in parameters {
                    if self.input_schema.pointer(pointer).is_some() {
                        text.parameters
                            .entry(pointer.clone())
                            .or_insert_with(|| description.clone());
                    }
                }
            }
            self.display = Some(ToolDisplayMetadata {
                default_locale: resource.default_locale.clone(),
                locales,
            });
        }
        self
    }
    pub fn resolve_display(&self, locale: &str) -> ResolvedToolDisplay {
        match &self.display {
            Some(display) => display.resolve(
                locale,
                &self.call_name,
                &self.description,
                &self.input_schema,
            ),
            None => ResolvedToolDisplay {
                name: self.call_name.clone(),
                description: self.description.clone(),
                input_schema: self.input_schema.clone(),
            },
        }
    }
}
