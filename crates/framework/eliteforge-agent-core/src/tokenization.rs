//! Local text tokenizers and deterministic, host-extensible model selection.
//! Protocol framing and media accounting belong to `TokenEstimator`.
use std::{collections::BTreeMap, sync::Arc};

/// Count plain text without adding conversation framing or special tokens.
pub trait TextTokenizer: Send + Sync {
    fn count_tokens(&self, text: &str) -> Result<usize, TokenizationError>;
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TokenizationError {
    #[error("invalid tokenizer registration: {0}")]
    Registration(String),
    #[error("tokenizer {tokenizer_id} failed: {message}")]
    Encoding {
        tokenizer_id: String,
        message: String,
    },
}

/// Accuracy describes text tokenization, never provider billing or full framing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenizerAccuracy {
    ModelTokenizer,
    FamilyEstimate,
    Heuristic,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenizerMatcher {
    Exact(String),
    Contains(String),
}

impl TokenizerMatcher {
    fn normalized(self) -> Self {
        match self {
            Self::Exact(name) => Self::Exact(normalize_model_name(&name)),
            Self::Contains(pattern) => Self::Contains(pattern.trim().to_ascii_lowercase()),
        }
    }

    fn pattern(&self) -> &str {
        match self {
            Self::Exact(pattern) | Self::Contains(pattern) => pattern,
        }
    }

    fn matches(&self, name: &str) -> bool {
        match self {
            Self::Exact(pattern) => name == pattern,
            Self::Contains(pattern) => name.contains(pattern),
        }
    }
}

/// A resolved counter is cheap to clone and remains stable for one model binding.
#[derive(Clone)]
pub struct TokenizerSelection {
    pub tokenizer_id: String,
    pub accuracy: TokenizerAccuracy,
    tokenizer: Arc<dyn TextTokenizer>,
}

impl TokenizerSelection {
    pub fn count_tokens(&self, text: &str) -> Result<usize, TokenizationError> {
        self.tokenizer.count_tokens(text)
    }
}

#[derive(Clone)]
struct TokenizerRule {
    matcher: TokenizerMatcher,
    tokenizer_id: String,
    accuracy: TokenizerAccuracy,
    priority: i32,
}

/// Build at host startup, then share immutable selections across model bindings.
/// Exact names win over family patterns. Family rules use priority, then longest
/// pattern, then registration order. Rebinding the same matcher replaces its rule.
#[derive(Clone)]
pub struct TokenizerRegistry {
    tokenizers: BTreeMap<String, Arc<dyn TextTokenizer>>,
    rules: Vec<TokenizerRule>,
    fallback: TokenizerSelection,
}

impl TokenizerRegistry {
    pub fn new(
        fallback_id: impl Into<String>,
        fallback: Arc<dyn TextTokenizer>,
    ) -> Result<Self, TokenizationError> {
        let fallback_id = fallback_id.into();
        validate_id(&fallback_id)?;
        Ok(Self {
            tokenizers: BTreeMap::from([(fallback_id.clone(), fallback.clone())]),
            rules: Vec::new(),
            fallback: TokenizerSelection {
                tokenizer_id: fallback_id,
                accuracy: TokenizerAccuracy::Heuristic,
                tokenizer: fallback,
            },
        })
    }

    pub fn register(
        &mut self,
        id: impl Into<String>,
        tokenizer: Arc<dyn TextTokenizer>,
    ) -> Result<(), TokenizationError> {
        let id = id.into();
        validate_id(&id)?;
        if self.tokenizers.contains_key(&id) {
            return Err(TokenizationError::Registration(format!(
                "duplicate tokenizer ID: {id}"
            )));
        }
        self.tokenizers.insert(id, tokenizer);
        Ok(())
    }

    pub fn bind(
        &mut self,
        matcher: TokenizerMatcher,
        tokenizer_id: &str,
        accuracy: TokenizerAccuracy,
        priority: i32,
    ) -> Result<(), TokenizationError> {
        if !self.tokenizers.contains_key(tokenizer_id) {
            return Err(TokenizationError::Registration(format!(
                "unknown tokenizer ID: {tokenizer_id}"
            )));
        }
        let matcher = matcher.normalized();
        if matcher.pattern().is_empty() {
            return Err(TokenizationError::Registration(
                "empty model matcher".into(),
            ));
        }
        let rule = TokenizerRule {
            matcher,
            tokenizer_id: tokenizer_id.into(),
            accuracy,
            priority,
        };
        if let Some(existing) = self
            .rules
            .iter_mut()
            .find(|existing| existing.matcher == rule.matcher)
        {
            *existing = rule;
        } else {
            self.rules.push(rule);
        }
        Ok(())
    }

    /// Ignore provider namespaces so `deepseek/another-model` does not select
    /// DeepSeek merely because of its provider. Match vendor names, not catalog IDs.
    pub fn select(&self, model_name: &str) -> TokenizerSelection {
        let name = normalize_model_name(model_name);
        let rule = self
            .rules
            .iter()
            .enumerate()
            .filter(|(_, rule)| rule.matcher.matches(&name))
            .max_by_key(|(index, rule)| {
                (
                    matches!(rule.matcher, TokenizerMatcher::Exact(_)),
                    rule.priority,
                    rule.matcher.pattern().len(),
                    std::cmp::Reverse(*index),
                )
            });
        match rule {
            Some((_, rule)) => TokenizerSelection {
                tokenizer_id: rule.tokenizer_id.clone(),
                accuracy: rule.accuracy,
                tokenizer: self.tokenizers[&rule.tokenizer_id].clone(),
            },
            None => self.fallback.clone(),
        }
    }
}

fn validate_id(id: &str) -> Result<(), TokenizationError> {
    if id.is_empty() || id.trim() != id {
        Err(TokenizationError::Registration(
            "tokenizer ID must be nonempty without surrounding whitespace".into(),
        ))
    } else {
        Ok(())
    }
}

fn normalize_model_name(name: &str) -> String {
    name.trim()
        .rsplit('/')
        .next()
        .unwrap_or(name)
        .trim()
        .to_ascii_lowercase()
}

/// Portable conservative fallback for unknown models and unpublished vocabularies.
pub struct UnicodeTextTokenizer;
impl TextTokenizer for UnicodeTextTokenizer {
    fn count_tokens(&self, text: &str) -> Result<usize, TokenizationError> {
        Ok(crate::estimate_unicode_text_tokens(text))
    }
}
