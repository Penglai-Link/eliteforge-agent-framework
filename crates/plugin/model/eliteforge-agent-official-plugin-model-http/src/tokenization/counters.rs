use base64::Engine;
use eliteforge_agent_core::{TextTokenizer, TokenizationError};
use std::{io::Read, sync::OnceLock};
use unicode_normalization::UnicodeNormalization;

/// Custom hosts may load their own Hugging Face JSON and register it in core.
pub struct HuggingFaceTextTokenizer(tokenizers::Tokenizer);
impl HuggingFaceTextTokenizer {
    pub fn from_json(json: &[u8]) -> Result<Self, TokenizationError> {
        let mut tokenizer =
            tokenizers::Tokenizer::from_bytes(json).map_err(|e| error("huggingface", e))?;
        // Upstream inference configs sometimes include truncation/padding. Local
        // accounting must count the whole input, including long context requests.
        tokenizer
            .with_truncation(None)
            .map_err(|e| error("huggingface", e))?;
        tokenizer.with_padding(None);
        Ok(Self(tokenizer))
    }
}
impl TextTokenizer for HuggingFaceTextTokenizer {
    fn count_tokens(&self, text: &str) -> Result<usize, TokenizationError> {
        self.0
            .encode(text, false)
            .map(|encoded| encoded.len())
            .map_err(|e| error("huggingface", e))
    }
}

pub(super) struct EmbeddedHuggingFaceTokenizer {
    id: &'static str,
    bytes: &'static [u8],
    tokenizer: OnceLock<Result<HuggingFaceTextTokenizer, TokenizationError>>,
}
impl EmbeddedHuggingFaceTokenizer {
    pub(super) fn new(id: &'static str, bytes: &'static [u8]) -> Self {
        Self {
            id,
            bytes,
            tokenizer: OnceLock::new(),
        }
    }
}
impl TextTokenizer for EmbeddedHuggingFaceTokenizer {
    fn count_tokens(&self, text: &str) -> Result<usize, TokenizationError> {
        self.tokenizer
            .get_or_init(|| HuggingFaceTextTokenizer::from_json(&unzip(self.id, self.bytes)?))
            .as_ref()
            .map_err(Clone::clone)?
            .count_tokens(text)
    }
}

pub(super) struct TikTokenTextTokenizer {
    id: &'static str,
    load: fn() -> Result<tiktoken_rs::CoreBPE, TokenizationError>,
    normalize: bool,
    tokenizer: OnceLock<Result<tiktoken_rs::CoreBPE, TokenizationError>>,
}
impl TikTokenTextTokenizer {
    fn new(
        id: &'static str,
        load: fn() -> Result<tiktoken_rs::CoreBPE, TokenizationError>,
        normalize: bool,
    ) -> Self {
        Self {
            id,
            load,
            normalize,
            tokenizer: OnceLock::new(),
        }
    }
    pub(super) fn o200k() -> Self {
        Self::new(
            "o200k_base",
            || tiktoken_rs::o200k_base().map_err(|e| error("o200k_base", e)),
            false,
        )
    }
    pub(super) fn cl100k() -> Self {
        Self::new(
            "cl100k_base",
            || tiktoken_rs::cl100k_base().map_err(|e| error("cl100k_base", e)),
            false,
        )
    }
    pub(super) fn kimi() -> Self {
        Self::new(
            "kimi-k2.5",
            || {
                let data = unzip(
                    "kimi-k2.5",
                    include_bytes!("../../resources/tokenizers/kimi-k2.5.tiktoken.model.gz"),
                )?;
                let ranks = std::str::from_utf8(&data)
                    .map_err(|e| error("kimi-k2.5", e))?
                    .lines()
                    .map(|line| {
                        let (token, rank) = line
                            .split_once(' ')
                            .ok_or_else(|| error("kimi-k2.5", "invalid BPE rank"))?;
                        Ok((
                            base64::engine::general_purpose::STANDARD
                                .decode(token)
                                .map_err(|e| error("kimi-k2.5", e))?,
                            rank.parse().map_err(|e| error("kimi-k2.5", e))?,
                        ))
                    })
                    .collect::<Result<_, TokenizationError>>()?;
                // The pre-tokenization expression published with Kimi-K2.5.
                let pattern = r"[\p{Han}]+|[^\r\n\p{L}\p{N}]?[\p{Lu}\p{Lt}\p{Lm}\p{Lo}\p{M}&&[^\p{Han}]]*[\p{Ll}\p{Lm}\p{Lo}\p{M}&&[^\p{Han}]]+(?i:'s|'t|'re|'ve|'m|'ll|'d)?|[^\r\n\p{L}\p{N}]?[\p{Lu}\p{Lt}\p{Lm}\p{Lo}\p{M}&&[^\p{Han}]]+[\p{Ll}\p{Lm}\p{Lo}\p{M}&&[^\p{Han}]]*(?i:'s|'t|'re|'ve|'m|'ll|'d)?|\p{N}{1,3}| ?[^\s\p{L}\p{N}]+[\r\n]*|\s*[\r\n]+|\s+(?!\S)|\s+";
                tiktoken_rs::CoreBPE::new(ranks, Default::default(), pattern)
                    .map_err(|e| error("kimi-k2.5", e))
            },
            false,
        )
    }
    pub(super) fn claude_legacy() -> Self {
        Self::new(
            "claude-legacy",
            || {
                let data = unzip(
                    "claude-legacy",
                    include_bytes!("../../resources/tokenizers/claude-legacy.json.gz"),
                )?;
                let value: serde_json::Value =
                    serde_json::from_slice(&data).map_err(|e| error("claude-legacy", e))?;
                let ranks = value["bpe_ranks"]
                    .as_str()
                    .ok_or_else(|| error("claude-legacy", "missing BPE ranks"))?;
                // js-tiktoken packs runs as `! <starting-rank> <base64> ...`.
                let mut tokens = ranks.split_whitespace();
                let mut rank = 0u32;
                let mut decoded = Vec::new();
                while let Some(token) = tokens.next() {
                    if token == "!" {
                        rank = tokens
                            .next()
                            .ok_or_else(|| error("claude-legacy", "missing rank"))?
                            .parse()
                            .map_err(|e| error("claude-legacy", e))?;
                        continue;
                    }
                    decoded.push((
                        base64::engine::general_purpose::STANDARD
                            .decode(token)
                            .map_err(|e| error("claude-legacy", e))?,
                        rank,
                    ));
                    rank += 1;
                }
                let ranks = decoded.into_iter().collect();
                let pattern = value["pat_str"]
                    .as_str()
                    .ok_or_else(|| error("claude-legacy", "missing pre-tokenizer"))?;
                tiktoken_rs::CoreBPE::new(ranks, Default::default(), pattern)
                    .map_err(|e| error("claude-legacy", e))
            },
            true,
        )
    }
}
impl TextTokenizer for TikTokenTextTokenizer {
    fn count_tokens(&self, text: &str) -> Result<usize, TokenizationError> {
        let tokenizer = self
            .tokenizer
            .get_or_init(self.load)
            .as_ref()
            .map_err(Clone::clone)?;
        let normalized;
        let text = if self.normalize {
            normalized = text.nfkc().collect::<String>();
            &normalized
        } else {
            text
        };
        let count = tokenizer.count_ordinary(text);
        // Keep tokenizer failures visible rather than silently claiming a different
        // vocabulary was used. Embedded dictionaries are validated by offline tests.
        if !text.is_empty() && count == 0 {
            return Err(error(self.id, "nonempty text produced no tokens"));
        }
        Ok(count)
    }
}

fn unzip(id: &str, bytes: &[u8]) -> Result<Vec<u8>, TokenizationError> {
    let mut json = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .read_to_end(&mut json)
        .map_err(|e| error(id, e))?;
    Ok(json)
}
fn error(id: &str, error: impl std::fmt::Display) -> TokenizationError {
    TokenizationError::Encoding {
        tokenizer_id: id.into(),
        message: error.to_string(),
    }
}
