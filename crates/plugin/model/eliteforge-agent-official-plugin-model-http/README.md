# Official HTTP model plugin

This plugin owns model wire protocols and single-attempt HTTP execution. It supports Chat Completions, Responses, embedding, native rerank, explicit chat-based rerank, and connectivity probes. Public native DTO conversion preserves images, tool identities, attachments, prepared schemas and Responses continuation data. The final Core-selected messages and output limit determine the HTTP body.

Use `ProtocolChatModel` with a Core `ModelConnection`, or use `HttpEmbeddingModel` / `HttpRerankModel` with the corresponding model service. `HttpProbeModel` and `ModelProbeKind` provide bounded status-only probes without host-owned protocol payloads. Host products supply configuration and a `ModelTransportFactory`; they retain account identity, authorized routing and credential policy.

`AgentModelConfig.supportsDeveloperRole` defaults to `false`. Enable it only for an endpoint that accepts developer instructions. Chat Completions sends origin-marked Agent and plugin instructions as `developer`; Responses puts only those instructions in its top-level `instructions` field. Other `system` messages retain their role in both protocols. The context assembler marks eligible origins, and the model bridge preserves that mark. This setting is independent of `thinkingEnabled`, which controls reasoning request fields.

`HttpModelTransport` and `PersonAiModelTransport` send one request per adapter dispatch and never retry or replace an empty stream with another request. PersonAI encryption and gateway headers use the official provider SDK. Framework owns HTTP execution and bounded byte-oriented SSE decoding. JSON bodies and aggregate streamed data are limited to 16 MiB; individual SSE frames are limited to 256 KiB. UTF-8 is decoded after frame assembly, with LF/CRLF and multiline data support. Incomplete output is indeterminate and cannot be retried after output.

Provider errors retain HTTP status, business code, message, category and optional host-supplied `authorization_attempt` identity. Ordinary HTTP 429 may be transient; PersonAI 429 means quota exhaustion and 404 means no available model. Account changes and permission/quota errors are terminal. Authentication recovery is a separate Framework-controlled action before another counted dispatch; factories must not send model requests from recovery hooks. A terminal hook receives the rejected model error, including its original attempt identity, even when credential refresh fails or times out.

Input budget estimates include encoded image bytes and native continuation metadata. Rerank estimates use the same native body constructor as dispatch, or the complete chat fallback prompt. Actual provider usage settles reservations; absent usage retains the conservative reservation in Core and remains absent in native product results.

Raw transport methods are integration primitives for adapters executing under a Framework invocation. Products execute independent chat, embedding and rerank directly through the existing `ModelClient` / `ModelOperations`, without constructing an Agent or native run, as described in [direct model calls](../../../../docs/受控模型作业.md).

模型 HTTP client 通过 SDK `ModelHttpClient::from_builder` 保留宿主 proxy/connect/pool/timeout 配置并强制禁止自动 redirect；307/308 是终态错误，配置应使用实际模型 endpoint，不能隐式再次 POST。PersonAI 官方 gateway request 加密与 header 仍保留。

## Local tokenizers

`tokenization::builtin_tokenizer_registry()` supplies offline Qwen, DeepSeek, GLM, Kimi, GPT, and Claude/Opus rules. The generic `TextTokenizer`, `TokenizerRegistry`, `TokenizerMatcher`, and `TokenizerAccuracy` contracts live in Core. Exact names override case-insensitive family containment rules; only the final vendor model-name path segment is matched. Unknown names use the conservative Unicode fallback. Family selection is not a promise that every version uses the same vocabulary. Opus uses Anthropic's legacy tokenizer only as an explicitly marked approximation; current Claude counts require the provider's model-aware counting API or returned usage.

Hosts can extend a clone without changing active model selections or the global defaults:

```rust,ignore
let mut registry = builtin_tokenizer_registry().clone();
registry.register("host.model-tokenizer", Arc::new(HuggingFaceTextTokenizer::from_json(&json)?))?;
registry.bind(
    TokenizerMatcher::Exact("vendor-model".into()),
    "host.model-tokenizer",
    TokenizerAccuracy::ModelTokenizer,
    0,
)?;
let selected = registry.select("provider/vendor-model");
let tokens = selected.count_tokens("hello")?;
```

Registration and selection perform no I/O or vocabulary parsing. Embedded counters initialize lazily and remain shared across registry clones. Imported Hugging Face dictionaries have truncation and padding disabled. Encoding failures are returned, not hidden by switching to another vocabulary. A `TextTokenizer` counts text; the host's `TokenEstimator` adds its actual message projection, media and protocol framing. Provider usage remains authoritative. [Pinned dictionaries and licenses](resources/tokenizers/README.md).
