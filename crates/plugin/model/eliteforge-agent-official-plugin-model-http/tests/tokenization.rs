use eliteforge_agent_core::{TextTokenizer, TokenizerAccuracy, TokenizerMatcher};
use eliteforge_agent_official_plugin_model_http::tokenization::{
    HuggingFaceTextTokenizer, builtin_tokenizer_registry,
};
use std::sync::Arc;

#[test]
fn selects_families_case_insensitively_with_specific_vocabularies() {
    let registry = builtin_tokenizer_registry();
    for (model, id) in [
        ("vendor/DeepSeek V4.1 Flash", "deepseek-v4.1-flash"),
        ("vendor/deepseek-v3.2", "deepseek-v4.1-flash"),
        ("vendor/GLM 5.3 Flash", "glm-5.3-flash"),
        ("vendor/GLM-4.7", "glm-5.3-flash"),
        ("vendor/QWEN-plus", "qwen3.5"),
        ("vendor/Qwen3-Coder", "qwen3"),
        ("vendor/Qwen3.5-Plus", "qwen3.5"),
        ("vendor/KIMI-K2.5", "kimi-k2.5"),
        ("vendor/moonshot-v1", "kimi-k2.5"),
        ("vendor/GPT-5.4", "o200k_base"),
        ("vendor/GPT-4o-mini", "o200k_base"),
        ("vendor/GPT-4.1-mini", "o200k_base"),
        ("vendor/GPT-4-0613", "cl100k_base"),
        ("vendor/GPT-3.5-Turbo", "cl100k_base"),
        ("vendor/OPUS-4.6", "claude-legacy"),
        ("vendor/Claude-Sonnet", "claude-legacy"),
        ("deepseek/opaque-model", "unicode_estimate"),
        ("catalog-uuid", "unicode_estimate"),
    ] {
        assert_eq!(registry.select(model).tokenizer_id, id, "{model}");
    }
    assert_eq!(
        registry.select("deepseek-v4.1-flash").accuracy,
        TokenizerAccuracy::ModelTokenizer
    );
    assert_eq!(
        registry.select("DeepSeek-new").accuracy,
        TokenizerAccuracy::FamilyEstimate
    );
    assert_eq!(
        registry.select("Claude-Opus-4.6").accuracy,
        TokenizerAccuracy::Heuristic
    );
}

#[test]
fn every_embedded_vocabulary_encodes_unicode_code_and_special_token_literals() {
    let registry = builtin_tokenizer_registry();
    for model in [
        "deepseek", "glm", "qwen3", "qwen3.5", "kimi", "gpt-4", "gpt-5", "opus",
    ] {
        let tokenizer = registry.select(model);
        assert_eq!(tokenizer.count_tokens("").unwrap(), 0, "{model}");
        for text in [
            "你好，世界！",
            "Hello world!",
            "fn main() { println!(\"你好\"); }",
            "👩‍💻 café e\u{301} مرحبا",
            "<|endoftext|> [EOS] <|im_end|>",
        ] {
            assert!(tokenizer.count_tokens(text).unwrap() > 0, "{model}: {text}");
        }
    }
}

#[test]
fn matches_independent_python_tokenizers_and_tiktoken_counts() {
    // Independently calculated with Python tokenizers 0.22.2 / tiktoken 0.14.0
    // and the pinned publisher assets listed in resources/tokenizers/README.md.
    let samples = [
        "你好，世界！",
        "Hello world!",
        "fn main() { println!(\"你好\"); }",
        "👩‍💻 café e\u{301} مرحبا",
        "<|endoftext|> [EOS] <|im_end|>",
    ];
    for (model, counts) in [
        ("deepseek", [4, 3, 9, 10, 17]),
        ("glm", [4, 3, 9, 8, 9]),
        ("qwen3", [4, 3, 9, 9, 6]),
        ("qwen3.5", [4, 3, 9, 11, 6]),
        ("kimi", [4, 3, 9, 12, 15]),
        ("gpt-4", [7, 3, 10, 15, 15]),
        ("gpt-5", [4, 3, 9, 10, 16]),
        ("opus", [7, 3, 12, 13, 17]),
    ] {
        let tokenizer = builtin_tokenizer_registry().select(model);
        for (text, expected) in samples.iter().zip(counts) {
            assert_eq!(
                tokenizer.count_tokens(text).unwrap(),
                expected,
                "{model}: {text}"
            );
        }
    }
}

#[test]
fn imported_tokenizers_never_truncate_or_pad_long_inputs() {
    let json = serde_json::json!({
        "version":"1.0",
        "truncation":{"direction":"Right", "max_length":2, "strategy":"LongestFirst", "stride":0},
        "padding":{"strategy":{"Fixed":16}, "direction":"Right", "pad_to_multiple_of":null, "pad_id":0, "pad_type_id":0, "pad_token":"[UNK]"},
        "added_tokens":[], "normalizer":null, "pre_tokenizer":{"type":"Whitespace"},
        "post_processor":null, "decoder":null,
        "model":{"type":"WordLevel", "vocab":{"[UNK]":0, "hello":1}, "unk_token":"[UNK]"}
    });
    let tokenizer =
        HuggingFaceTextTokenizer::from_json(&serde_json::to_vec(&json).unwrap()).unwrap();
    assert_eq!(tokenizer.count_tokens("hello").unwrap(), 1);
    assert_eq!(
        tokenizer.count_tokens(&"hello ".repeat(100_000)).unwrap(),
        100_000
    );
    assert!(HuggingFaceTextTokenizer::from_json(b"invalid").is_err());
}

#[test]
fn host_can_register_a_counter_without_changing_builtins_or_active_selection() {
    let mut registry = builtin_tokenizer_registry().clone();
    let json = br#"{"version":"1.0","model":{"type":"WordLevel","vocab":{"[UNK]":0,"host text":1},"unk_token":"[UNK]"}}"#;
    registry
        .register(
            "host-tokenizer",
            Arc::new(HuggingFaceTextTokenizer::from_json(json).unwrap()),
        )
        .unwrap();
    registry
        .bind(
            TokenizerMatcher::Exact("qwen-host".into()),
            "host-tokenizer",
            TokenizerAccuracy::ModelTokenizer,
            0,
        )
        .unwrap();
    assert_eq!(
        registry
            .select("QWEN-host")
            .count_tokens("host text")
            .unwrap(),
        1
    );
    assert_eq!(
        builtin_tokenizer_registry()
            .select("QWEN-host")
            .tokenizer_id,
        "qwen3.5"
    );
}
