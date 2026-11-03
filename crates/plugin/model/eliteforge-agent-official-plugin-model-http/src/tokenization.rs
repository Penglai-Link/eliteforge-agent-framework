//! Offline tokenizer implementations and extensible default model-family rules.
//! Hosts can clone the registry and add exact model bindings without changing core.
mod counters;
pub use counters::HuggingFaceTextTokenizer;
use counters::{EmbeddedHuggingFaceTokenizer, TikTokenTextTokenizer};
use eliteforge_agent_core::{
    TokenizerAccuracy as Accuracy, TokenizerMatcher as Matcher, TokenizerRegistry,
    UnicodeTextTokenizer,
};
use std::sync::{Arc, OnceLock};

/// All vocabularies are embedded; registration does not parse them or use the
/// network. Each vocabulary is initialized only on its first token count.
pub fn builtin_tokenizer_registry() -> &'static TokenizerRegistry {
    static REGISTRY: OnceLock<TokenizerRegistry> = OnceLock::new();
    REGISTRY.get_or_init(build_registry)
}

fn build_registry() -> TokenizerRegistry {
    let mut registry =
        TokenizerRegistry::new("unicode_estimate", Arc::new(UnicodeTextTokenizer)).unwrap();
    for (id, bytes) in [
        (
            "deepseek-v4.1-flash",
            include_bytes!("../resources/tokenizers/deepseek-v4.1-flash.tokenizer.json.gz")
                .as_slice(),
        ),
        (
            "glm-5.3-flash",
            include_bytes!("../resources/tokenizers/glm-5.3-flash.tokenizer.json.gz").as_slice(),
        ),
        (
            "qwen3",
            include_bytes!("../resources/tokenizers/qwen3.tokenizer.json.gz").as_slice(),
        ),
        (
            "qwen3.5",
            include_bytes!("../resources/tokenizers/qwen3.5.tokenizer.json.gz").as_slice(),
        ),
    ] {
        registry
            .register(id, Arc::new(EmbeddedHuggingFaceTokenizer::new(id, bytes)))
            .unwrap();
    }
    for (id, tokenizer) in [
        ("o200k_base", TikTokenTextTokenizer::o200k()),
        ("cl100k_base", TikTokenTextTokenizer::cl100k()),
        ("kimi-k2.5", TikTokenTextTokenizer::kimi()),
        ("claude-legacy", TikTokenTextTokenizer::claude_legacy()),
    ] {
        registry.register(id, Arc::new(tokenizer)).unwrap();
    }
    // The family is intentionally fuzzy and case-insensitive. A shared family
    // vocabulary is an estimate for unverified versions, not a claimed exact count.
    for (pattern, id, accuracy) in [
        ("deepseek", "deepseek-v4.1-flash", Accuracy::FamilyEstimate),
        ("glm", "glm-5.3-flash", Accuracy::FamilyEstimate),
        ("qwen", "qwen3.5", Accuracy::FamilyEstimate),
        ("qwen2", "qwen3", Accuracy::FamilyEstimate),
        ("qwen3", "qwen3", Accuracy::FamilyEstimate),
        ("qwen3.5", "qwen3.5", Accuracy::FamilyEstimate),
        ("kimi", "kimi-k2.5", Accuracy::FamilyEstimate),
        ("moonshot", "kimi-k2.5", Accuracy::FamilyEstimate),
        ("gpt", "o200k_base", Accuracy::FamilyEstimate),
        ("gpt-3.5", "cl100k_base", Accuracy::FamilyEstimate),
        ("gpt-35", "cl100k_base", Accuracy::FamilyEstimate),
        ("gpt-4", "cl100k_base", Accuracy::FamilyEstimate),
        ("gpt-4o", "o200k_base", Accuracy::FamilyEstimate),
        ("gpt-4.1", "o200k_base", Accuracy::FamilyEstimate),
        ("gpt-4.5", "o200k_base", Accuracy::FamilyEstimate),
        ("opus", "claude-legacy", Accuracy::Heuristic),
        ("claude", "claude-legacy", Accuracy::Heuristic),
    ] {
        registry
            .bind(Matcher::Contains(pattern.into()), id, accuracy, 0)
            .unwrap();
    }
    for (model, id) in [
        ("deepseek-v4.1-flash", "deepseek-v4.1-flash"),
        ("deepseek-v4.1-flash-1017", "deepseek-v4.1-flash"),
        ("glm-5.3-flash", "glm-5.3-flash"),
        ("qwen3-8b", "qwen3"),
        ("qwen3.5-9b", "qwen3.5"),
        ("kimi-k2.5", "kimi-k2.5"),
        ("gpt-4", "cl100k_base"),
        ("gpt-3.5-turbo", "cl100k_base"),
        ("gpt-4o", "o200k_base"),
        ("gpt-4o-mini", "o200k_base"),
        ("gpt-4.1", "o200k_base"),
        ("gpt-4.1-mini", "o200k_base"),
        ("gpt-5", "o200k_base"),
        ("o1", "o200k_base"),
        ("o3", "o200k_base"),
        ("o4-mini", "o200k_base"),
    ] {
        registry
            .bind(
                Matcher::Exact(model.into()),
                id,
                Accuracy::ModelTokenizer,
                0,
            )
            .unwrap();
    }
    registry
}
