# Official HTTP model plugin

This plugin owns model wire protocols and single-attempt HTTP execution. It supports Chat Completions, Responses, embedding, native rerank, explicit chat-based rerank, and connectivity probes. Public native DTO conversion preserves images, tool identities, attachments, prepared schemas and Responses continuation data. The final Core-selected messages and output limit determine the HTTP body.

Use `ProtocolChatModel` with a Core `ModelConnection`, or use `HttpEmbeddingModel` / `HttpRerankModel` with the corresponding model service. `HttpProbeModel` and `ModelProbeKind` provide bounded status-only probes without host-owned protocol payloads. Host products supply configuration and a `ModelTransportFactory`; they retain account identity, authorized routing and credential policy.

`HttpModelTransport` and `PersonAiModelTransport` send one request per adapter dispatch and never retry or replace an empty stream with another request. PersonAI encryption and gateway headers use the official provider SDK. Framework owns HTTP execution and bounded byte-oriented SSE decoding. JSON bodies and aggregate streamed data are limited to 16 MiB; individual SSE frames are limited to 256 KiB. UTF-8 is decoded after frame assembly, with LF/CRLF and multiline data support. Incomplete output is indeterminate and cannot be retried after output.

Provider errors retain HTTP status, business code, message, category and optional host-supplied `authorization_attempt` identity. Ordinary HTTP 429 may be transient; PersonAI 429 means quota exhaustion and 404 means no available model. Account changes and permission/quota errors are terminal. Authentication recovery is a separate Framework-controlled action before another counted dispatch; factories must not send model requests from recovery hooks. A terminal hook receives the rejected model error, including its original attempt identity, even when credential refresh fails or times out.

Input budget estimates include encoded image bytes and native continuation metadata. Rerank estimates use the same native body constructor as dispatch, or the complete chat fallback prompt. Actual provider usage settles reservations; absent usage retains the conservative reservation in Core and remains absent in native product results.

Raw transport methods are integration primitives for adapters executing under a Framework invocation. Products should submit independent work through `ModelJobClient`, as described in [controlled model jobs](../../../../docs/受控模型作业.md).

模型 HTTP client 通过 SDK `ModelHttpClient::from_builder` 保留宿主 proxy/connect/pool/timeout 配置并强制禁止自动 redirect；307/308 是终态错误，配置应使用实际模型 endpoint，不能隐式再次 POST。PersonAI 官方 gateway request 加密与 header 仍保留。
