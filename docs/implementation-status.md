# Implementation status and acceptance evidence

The requested delivery comprises independent Framework contracts, varied execution
scenarios, and reference plugins. The implementation is independent of Tron;
no Tron compatibility rules were added to the execution contracts.

## Workspace

- Repository: `eliteforge-agent-framework`
- Branch: `feature/clouds3n/agent-spawn-threads`
- Base commit: `b85e050`; the current Agent Spawn and Threads delivery is in the working tree.
- No Tron files, Git branches, commits or remote state were changed.
- Validation environment: macOS arm64, Rust/Cargo 1.95.0.

## Acceptance map

| Required area | Implemented behavior | Executable evidence |
| --- | --- | --- |
| Messages and results | Ordered typed content, structured tool results, generation options, offline JSON Schema validation | `eliteforge-agent-framework/tests/execution_contracts.rs`; `eliteforge-agent-official-plugin-model-genai/tests/protocol.rs` |
| Shared model connections and strategies | Shared transport/authentication with optional per-model protocol bindings; explicit request, stage, operation default and unique compatible candidate selection; fixed/stage/failover policies | `eliteforge-agent-framework/tests/model_connections.rs`; `eliteforge-agent-framework/tests/model_and_tool_controls.rs`; `eliteforge-agent-state-sqlite/tests/workflow.rs` |
| Core and plugin boundaries | Plugins depend on eliteforge-agent-core; Core reexports plugin authoring contracts and depends on generic kernel; optional default Genai dependency points outward from Framework | Production Cargo dependency inspection; `eliteforge-agent-framework/tests/default_genai.rs`; no-default-feature Framework tests |
| Typed model operations | Embedding, rerank, image/video generation, speech recognition/synthesis and realtime registrations; operation-specific requests, output checks, model binding and lifecycle cancellation | `eliteforge-agent-framework/tests/model_capabilities.rs` |
| Genai protocols | Chat Completions and Responses can coexist in one connection; native Gemini audio/video; distinct OpenAI/Gemini/Cohere embedding calls; scoped continuation | `eliteforge-agent-official-plugin-model-genai/tests/protocol.rs`; continuation isolation test in `src/lib.rs` |
| Embedding space and budget | Batch index/dimension/finite-value checks; explicit space revision and preprocessing identity; conservative shared-run usage when tokens are unknown | `eliteforge-agent-framework/tests/model_capabilities.rs` |
| Extension contracts | Separate lifecycle hook contributions and ordered run/node/model/tool interceptors; bounded hook failure isolation, immutable run/tool continuations, per-step context contributions and terminal contract checks | `eliteforge-agent-framework/tests/hooks_and_interceptors.rs`; `eliteforge-agent-framework/tests/execution_contracts.rs`; `eliteforge-agent-framework/tests/host_contract.rs` |
| Token estimation and admission | Connection-level model-aware estimator throughout built-in planning and final admission; atomic reservation, conservative unknown usage, JSON-output capability check | `eliteforge-agent-framework/tests/model_and_tool_controls.rs`; `eliteforge-agent-framework/tests/execution_contracts.rs` |
| Resource lifecycle | Async reverse-order shutdown, sync Drop fallback, bounded workflow lease release, installation and ready rollback; execution and cleanup errors retained together; cancelled close waiter cannot abandon cleanup | `eliteforge-agent-plugin-kernel/tests/async_shutdown.rs`; `eliteforge-agent-plugin-kernel/tests/container.rs`; `eliteforge-agent-framework/tests/async_lifecycle.rs`; release-fault tests in `eliteforge-agent-state-sqlite/tests/workflow.rs` |
| Tools and run control | Schema before effects; bounded opt-in ReAct parallelism; cloneable RunControl independent of result waiting, owner-drop cancellation, tool revocation at dispatch; post-effect middleware failure terminates the run | `eliteforge-agent-framework/tests/runtime_control.rs`; `eliteforge-agent-framework/tests/model_and_tool_controls.rs`; `eliteforge-agent-state-sqlite/tests/workflow.rs` |
| Filesystem and system-command capability packs | Explicit directory-confined read/search and write/patch packs; an independent cross-platform system-command pack; absolute paths, `..`, and symlink traversal are rejected only for filesystem tools | `eliteforge-agent-capability-tools/tests/capability_packs.rs` |
| Agent Skills | Core declares storage-neutral `SkillProvider`; selected capability packs can contribute embedded resources, directories and declared Providers; one runtime aggregates catalogs, routes loading/resources/releases, applies merged selection, and exposes one progressive `skill_load` / `skill_read_resource` tool set | `eliteforge-agent-official-plugin-skills/tests/skills.rs`; `eliteforge-agent-framework/tests/capability_packs.rs` |
| Independent execution scenarios | Simple chat, tools, configurable stages, input, approval, nested child Agents through real invokers | `eliteforge-agent-framework/tests/host_contract.rs`; `eliteforge-agent-framework/tests/delegation.rs`; `eliteforge-agent-state-sqlite/tests/workflow.rs` |
| Child execution | Root/parent identity, inherited limits, upfront allowance; rejected/interrupted admission refunds local budget/count; confirmed reservations corrected after rejected dispatch; unknown writes remain conservative; official `agent_spawn` adapter | `eliteforge-agent-framework/tests/delegation.rs`; `eliteforge-agent-official-plugin-tools-agents/tests/tools.rs`; admission journal fault tests in `eliteforge-agent-framework/src/tests.rs`; durable child budget/count test in `eliteforge-agent-state-sqlite/tests/workflow.rs` |
| Independent thread collaboration | Official `thread_create/send/answer/wait/read/list/close` tools; persisted interaction answer and exact-run resume; exhaustive terminal status mapping; pre-dispatch bounded registry admission and explicit capacity release; replaceable core runtime contract | `eliteforge-agent-official-plugin-tools-agents/tests/tools.rs`; `eliteforge-agent-core/src/collaboration.rs`; `docs/collaboration.md` |
| Persistence | Atomic checkpoint/transcript/journal; session-head predecessor checks, CAS revisions; stable fencing identity across renewals with authoritative store expiry; bounded worker admission, WAL and FULL synchronous mode | `eliteforge-agent-state-sqlite/tests/store.rs`; `eliteforge-agent-state-sqlite/src/lib.rs` cancellation test |
| Persistent questions and answers | Agent pending_interactions / interaction / answer over shared InteractionStore; structured AskUserTool, append-only answer messages; exact ReAct approval; AG-UI interrupt and next-execution resume across restart | `eliteforge-agent-official-plugin-storage-sqlite/tests/interactions.rs`; `tests/stores.rs` |
| SQLite default storage | One explicit file for six storage roles, immutable binary artifacts, no implicit in-memory fallback, reverse-order owned connection cleanup | `eliteforge-agent-official-plugin-storage-sqlite/tests/stores.rs`; `tests/interactions.rs` |
| Native SQLite extensions | Independent eliteforge-agent-sqlite-extensions package, target/ABI resource selection, pinned SHA-256, atomic content-checked extraction, connection-local loading and explicit capability reports | `eliteforge-agent-sqlite-extensions/src/lib.rs`; all-feature `eliteforge-agent-official-plugin-storage-sqlite/tests/stores.rs` |
| Durable execution and approval | Intent and per-call result journal; exact approved descriptor/arguments; typed answers; terminal replay without tools | `eliteforge-agent-state-sqlite/tests/workflow.rs`; `eliteforge-agent-state-sqlite/tests/crash_recovery.rs` |
| DAG execution and plugins | petgraph 0.8.3 topology validation; contribution-based node registry/configuration schemas; bounded Tokio JoinSet; selected/all joins and skip propagation; atomic node result/route persistence; embedded ReAct checkpoints | `eliteforge-agent-state-sqlite/tests/dag.rs`; 2 additional DAG process-kill scenarios in `tests/crash_recovery.rs`; offline `examples/dag.rs` |
| Shared DAG controls | ReAct nodes share token, step and tool-count admission; run-wide tool concurrency and exclusive unsafe-tool dispatch; independently addressed approvals; uncertain storage stops progression | Parallel budget, approval, safety, limits and injected-storage-fault cases in `eliteforge-agent-state-sqlite/tests/dag.rs` |
| Interruption and failure boundaries | Real executor process kill, in-flight budget reservation, completed/pending tool siblings, simultaneous resume, definition drift after optimistic read, schema drift, policy failure, SQL completion after await cancellation | `eliteforge-agent-state-sqlite/tests/crash_recovery.rs`; `eliteforge-agent-state-sqlite/tests/workflow.rs`; `eliteforge-agent-state-sqlite/src/lib.rs` |
| Event semantics | Distinct live execution-attempt and durable-journal scopes; serialized publication; workflow steering messages and events committed together before live publication, including final replies; typed child-Agent and independent-Thread lifecycle events; bounded normal-event drain followed by a separately bounded terminal attempt per sink | `eliteforge-agent-framework/tests/execution_contracts.rs`; `eliteforge-agent-official-plugin-tools-agents/tests/tools.rs`; steering/replay/failure and acquired-checkpoint/event-scope tests in `eliteforge-agent-state-sqlite/tests/workflow.rs` |
| Agent plugin operations | Plugins configure services and per-run output factories; Agent owns execution, controls, model operations, storage requests and shutdown admission; typed custom operations and output ownership checks | `eliteforge-agent-framework/tests/agent_operations.rs`; storage bundle and MCP transport tests |
| Capability packs | Core exports `CapabilityPack` / builder, `#[extension_point]`, `#[capability]` and `#[capability_factory]`; selected packs and explicit children resolve static descriptors, preflight IDs/dependencies/cycles, create sync configured instances in stable order, share declared instances, prepare Skills resources and enter install-then-ready Plugin lifecycle | `eliteforge-agent-framework/tests/capability_packs.rs`; cross-crate declarations in `eliteforge-agent-capability-test-fixture/src/lib.rs`; resolver unit coverage in `eliteforge-agent-core/src/capability_packs.rs` |
| Plugin packs | AgentBuilder::plugin_pack expands custom, nested and official packs into one dependency graph; official defaults register eight plugins: Genai, SQLite, human tools, Agent collaboration tools, Skills, AG-UI, empty MCP and telemetry using upper-application global providers | `eliteforge-agent-framework/tests/plugin_packs.rs`; `eliteforge-agent-official-plugin-pack/tests/official_pack.rs`; `eliteforge-agent-official-plugin-tools-agents/tests/tools.rs`; `eliteforge-agent-official-plugin-skills/tests/skills.rs`; `tests/global_telemetry.rs`; offline `examples/official_pack.rs` |
| AG-UI output | Community ag-ui 0.3.0 event types and SSE; ordered bounded handoff independent of ordinary observer backlog; text/reasoning/tool/step/state/activity/custom events; native subagent lifecycle plus independent-thread custom events; distinct model/tool invocation identities, offloaded result references, tool-bound approval/denial across executor restart, final-response deduplication and serialized admission checks; explicit overflow/size/order errors | 23 SDK-client and lifecycle cases in `eliteforge-agent-official-plugin-agui/tests/protocol.rs`; offline `eliteforge-agent-official-plugin-agui/examples/sse.rs` |
| genai | Exact `=0.7.0-beta.21`, real local HTTP/SSE, shared auth, model discovery, tools, signed-thinking continuation, media input and generation options | `eliteforge-agent-official-plugin-model-genai/tests/protocol.rs` |
| MCP | Official rmcp 3.2.0 stdio and Streamable HTTP, bounded discovery/cursor detection, typed errors/results, protocol cancellation on cancel/drop, owned/shared close | `eliteforge-agent-official-plugin-tools-mcp/tests/transport.rs` launches actual SDK services over HTTP and in a child process |
| Conversation storage and windows | SQLite conversations, branch/generation identity, versioned atomic batches; independent context quotas and explicit layout; no source-history trimming | `eliteforge-agent-framework/tests/context_windows.rs`; `eliteforge-agent-memory/tests/stores.rs` |
| Long-term memory | Versioned/expiring records, explicit writes, bounded keyword search, fresh context retrieval, upper-application-scoped tool namespace | `eliteforge-agent-memory/tests/stores.rs`; `eliteforge-agent-memory/tests/components.rs` |
| Summary persistence | Model-aware packing, SQLite CAS store, range fingerprints, incremental reuse after reopen, branch/policy isolation | `eliteforge-agent-context/tests/compression.rs`; `summary_store.rs`; `eliteforge-agent-memory/tests/components.rs` |
| Node conversation records | Atomic node-state and interaction-batch recording, stable history across resume, explicit layout across all three loops | `eliteforge-agent-state-sqlite/tests/context_windows.rs` |
| Compression | Model-backed auxiliary calls use the same middleware/budget journal, silent summary deltas, preserved latest signed/tool turn, explicit failure on unusable summaries; raw history unchanged | `eliteforge-agent-context/tests/compression.rs` covers both ReAct and SQLite Workflow projections |
| Artifacts | Directory capability, canonical random IDs, atomic immutable publication, fsync, bounded byte ranges, cancellation, symlink rejection, combined content/structured offload, explicit result reference, artifact preservation after publication/history failures | `eliteforge-agent-context/src/artifact.rs`; `eliteforge-agent-context/tests/artifacts.rs` |
| OpenTelemetry | Injected providers; run/node/model/tool spans, node-scoped model/tool links, persisted attempt identities across continuation, bounded redacted error events, duration/usage/error/cancellation metrics; inline assembly/failure/dispatch diagnostics while spans are alive; no payloads by default; providers remain owned by the upper application | `eliteforge-agent-official-plugin-otel/tests/export.rs` uses actual SDK in-memory exporters |

## Current verification

- `cargo test -p eliteforge-agent-capability-macros -p eliteforge-agent-core`: passed.
- `cargo test -p eliteforge-agent-framework --test capability_packs`: 31 passed, covering external extension declarations, singleton conflicts, factory bindings, Context storage adapters, `ready` rollback and capability-owned Skills sources.
- `cargo test -p eliteforge-agent-official-plugin-skills`: 23 passed, including aggregate Provider routing, duplicate names, embedded-resource path/digest validation, concurrent preparation and version coexistence.
- `cargo test -p eliteforge-agent-plugin-kernel`: 17 passed, including install-then-ready ordering and ready-phase rollback.
- `cargo test -p eliteforge-agent-context -p eliteforge-agent-official-plugin-storage-sqlite`: 36 passed.
- `make test`: passed, including workspace all-target tests, documentation tests and the Framework no-default-features suite.
- `make lint`: passed, including format check, Clippy with `-D warnings`, compilation and Rustdoc.

The cross-crate suite covers selected and unselected packs, explicit children, stable dependency order,
shared multi-interface instances, per-Agent isolation, pre-factory rejection of ID/dependency/cycle
errors, compatibility with legacy plugins, reserved framework IDs, and rollback after Plugin installation failure.
Review regressions additionally cover cross-module and aliased implementation/configuration types,
implicit and explicit shared-instance keys, distinct factory conflicts, stacked declarations of two
instances, combined ordinary-plugin/capability cycles and missing dependencies, and factory ordering
through ordinary-plugin dependency edges.

Core now declares a version for its capability-macro dependency. Offline packaging with
`--registry crates-io --allow-dirty --no-verify` produced the macro and kernel archives;
Core passed the version-requirement check but could not find the macro package in the configured
`panamax-sparse` replacement index while offline. No package was uploaded. End-to-end Core
packaging still requires these dependency versions to be available in the target registry.

The same registry condition currently blocks `cargo package --offline --allow-dirty --no-verify`
for `eliteforge-agent-official-plugin-skills`: Cargo cannot resolve the published
`eliteforge-agent-core` package from `panamax-sparse`. The normal locked workspace build uses the
local path dependency and passes; no Skills archive was uploaded.

## Historical baseline verification

The following evidence belongs to the earlier baseline delivery and predates the current
capability-pack dependency additions. It is retained for the broader runtime scope; use the
section above for this change's current validation result.

- `cargo test --workspace --all-targets --all-features`: 249 tests passed, including optional native SQLite extension checks and local model protocol fixtures.
- `cargo test -p eliteforge-agent-framework --no-default-features --all-targets`: 87 tests passed; the Framework can run without the Genai integration.
- Model plugin HTTP/SSE tests use loopback fixtures, including fragmented responses, Chat Completions, Responses, native Gemini media and text embeddings. No live provider credentials or paid model calls were used.
- `cargo fmt --all -- --check`: passed.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: passed.
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps`: passed.
- `cargo test --workspace --doc --all-features`: passed; there are currently no Rust doctest cases.
- This delivery adds the Skills crate plus `serde_norway` and `unsafe-libyaml-norway` to
  Cargo.lock; no existing package version or checksum was rewritten.
- Cargo checks run from `/tmp` with a temporary `CARGO_HOME` that reuses the local cache. This avoids the unavailable configured mirror without changing repository or global Cargo configuration.
- `git diff --check` and an additional source/document whitespace scan cover tracked and new files. Local Markdown file links are checked after moving the public contracts into Core.

## Explicit behavior and validation limits

- SQLite implementations live in component and plugin crates; the independent eliteforge-agent-sqlite-extensions package handles optional native extensions. Core owns the storage and context contracts.
- Native extension execution was verified on macOS arm64 only. Other resources were checksum-verified; OS/CPU/ABI mapping tests do not establish runtime compatibility on those systems. Default storage does not require either extension.
- Long-term memory defaults to keyword matching, with no automatic fact extraction, vector search or retrieval-content cache.
- Artifact retention defaults to indefinite storage. The upper application collects authoritative references and schedules deletion only after quiescing writes and checking stored references; no background GC is implied.
- The `memory_context` example compiles; it requires a real configured API to run and was not sent to a live provider.
- No live Anthropic account was used. Local signed-thinking fixtures demonstrate
  adapter serialization and parsing, not live-provider certification.
- ReAct, WorkflowLoop and built-in DAG ReAct nodes share persistent model/tool execution.
  Tool batches run with bounded parallelism only when every pending tool is explicitly marked safe;
  mixed or unsafe batches run serially. Cross-node dispatch also respects the shared concurrency limit.
  DAG is selected through AgentLoopPlugin; the default Agent loop remains ReAct.
- DAG uses addressed interactions instead of ambiguous run-wide text steering.
  It has no cyclic edges or distributed worker queue. Custom node implementations
  are trusted cooperative code and must enforce the advertised invocation contracts.
- A Started external call without a saved result fails the run and is not automatically repeated.
  Exactly-once effects require external idempotency; the framework does not provide this guarantee.
- `run_child` creates a new execution and rejects implicit reparenting/resume of
  existing durable child executions. Cross-process child orchestration remains
  an explicit upper-application responsibility, not a hidden in-process guarantee.
- The default Thread runtime is intentionally process-local and bounded to 128
  records. It does not provide cross-process queueing, registry recovery, identity
  authorization or worktree isolation; those belong to an injected
  `AgentCollaborationRuntime` implementation in the upper application. Finished or
  awaiting-input records retain history until `thread_close` explicitly releases
  their registry capacity; persisted conversation and interaction retention remains
  owned by the configured stores.
- The reference compressor uses conservative estimates; tokenizer-specific
  custom reducers can implement ContextReducer. A summary can lose semantic
  detail, so raw history remains available and is never replaced by the summary.
- Artifact offload persists the complete content and structured result, retaining status,
interaction effects and an explicit result reference inline. The upper application owns artifact authorization and retention.
- MCP supports tool discovery/calls, not the entire MCP feature set. Decoded
  results and SSE event sizes are bounded; stdio/plain HTTP JSON decoding peak
  memory depends on the SDK. No handwritten MCP protocol engine was introduced.
- genai's text-only tool-result limitation and Developer-to-System mapping remain
  explicit. Unknown media estimates fail closed unless an estimator supplies them.
- Live streams are best effort. Durable journal sequence numbers are a distinct
  namespace; consumers cannot treat a live cursor as a durable replay cursor.
- AG-UI has its own ordered bounded channel. Incomplete delivery terminates with
  RUN_ERROR; it does not imply execution failure or retry effects. Application
  HTTP endpoints, request authentication, persistent replay, client-side tools,
  history import and merged SUBAGENT streams are outside this adapter. Existing
  workflow journals cannot reconstruct a complete token stream.
- Explicitly shared SQLite, MCP and telemetry infrastructure is owned by the upper application. The SQLite bundle
  owns and closes the connections it creates. This work does
  not claim account-level subscription billing, cross-node quota coordination,
  arbitrary plugin sandboxing, lowest-Rust-version certification or load certification.
- Capability-pack factories are synchronous and only receive their selected pack configuration.
  They do not resolve services, perform async initialization or provide member-level switches.
  Official `PluginPack` composition is intentionally unchanged; it remains the supported
  official-plugin entry point.

The execution invariants and acceptance scope are retained in [execution-contracts.md](execution-contracts.md). Current layers and defaults are described in [architecture.md](architecture.md), with the extension inventory in [plugins.md](plugins.md).

- Genai registers Chat and text Embedding only. The other typed model operations have tested runtime dispatch and extension contracts; applications supply the corresponding provider plugins.
- EmbeddingSpace validates configured identity, not the remote weights behind mutable provider aliases. The upper application pins the model revision and applies the declared preprocessing.
- Independent operation timeout/cancellation drops local invocation futures. Remote jobs may continue; video cancellation is explicit and requires provider support. Image/video/speech billing is not represented as Chat token usage.
