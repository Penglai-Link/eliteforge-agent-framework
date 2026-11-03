use eliteforge_agent_core::*;
use std::sync::Arc;

#[test]
fn registration_selection_and_exact_host_override() {
    let mut registry = TokenizerRegistry::new("fallback", Arc::new(UnicodeTextTokenizer)).unwrap();
    for id in ["family", "special", "host"] {
        registry
            .register(id, Arc::new(UnicodeTextTokenizer))
            .unwrap();
    }
    registry
        .bind(
            TokenizerMatcher::Contains("GLM".into()),
            "family",
            TokenizerAccuracy::FamilyEstimate,
            0,
        )
        .unwrap();
    registry
        .bind(
            TokenizerMatcher::Contains("glm-5".into()),
            "special",
            TokenizerAccuracy::FamilyEstimate,
            0,
        )
        .unwrap();
    registry
        .bind(
            TokenizerMatcher::Exact("GLM-5-custom".into()),
            "host",
            TokenizerAccuracy::ModelTokenizer,
            -100,
        )
        .unwrap();
    assert_eq!(
        registry.select("provider/GlM-5-CUSTOM").tokenizer_id,
        "host"
    );
    assert_eq!(
        registry.select("provider/GLM-5-new").tokenizer_id,
        "special"
    );
    assert_eq!(registry.select("some GLM model").tokenizer_id, "family");
    assert_eq!(registry.select("glm/unrelated").tokenizer_id, "fallback");
    assert_eq!(
        registry.select("opaque-catalog-uuid").accuracy,
        TokenizerAccuracy::Heuristic
    );
    // Host changes the rule without mutating a selection held by an active run.
    let old = registry.select("glm-5-custom");
    registry
        .bind(
            TokenizerMatcher::Exact("glm-5-custom".into()),
            "family",
            TokenizerAccuracy::FamilyEstimate,
            0,
        )
        .unwrap();
    assert_eq!(registry.select("glm-5-custom").tokenizer_id, "family");
    assert_eq!(old.tokenizer_id, "host");
    assert_eq!(old.count_tokens("中文").unwrap(), 4);
}

#[test]
fn validates_registration_and_respects_priority() {
    let mut registry = TokenizerRegistry::new("fallback", Arc::new(UnicodeTextTokenizer)).unwrap();
    assert!(
        registry
            .register("fallback", Arc::new(UnicodeTextTokenizer))
            .is_err()
    );
    assert!(
        registry
            .register(" ", Arc::new(UnicodeTextTokenizer))
            .is_err()
    );
    assert!(
        registry
            .bind(
                TokenizerMatcher::Contains("".into()),
                "fallback",
                TokenizerAccuracy::Heuristic,
                0
            )
            .is_err()
    );
    assert!(
        registry
            .bind(
                TokenizerMatcher::Contains("a".into()),
                "missing",
                TokenizerAccuracy::Heuristic,
                0
            )
            .is_err()
    );
    registry
        .register("host", Arc::new(UnicodeTextTokenizer))
        .unwrap();
    registry
        .bind(
            TokenizerMatcher::Contains("glm-5".into()),
            "fallback",
            TokenizerAccuracy::FamilyEstimate,
            0,
        )
        .unwrap();
    registry
        .bind(
            TokenizerMatcher::Contains("glm".into()),
            "host",
            TokenizerAccuracy::FamilyEstimate,
            10,
        )
        .unwrap();
    assert_eq!(registry.select("glm-5").tokenizer_id, "host");
}
