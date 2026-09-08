# Implementation status and acceptance evidence

The requested delivery comprises independent Framework contracts, varied execution
scenarios, and reference plugins. The implementation is independent of Tron;
no Tron compatibility rules were added to the execution contracts.

## Workspace

- Worktree: `/Users/clouds3n/Coding/open-source/ai/agent-framework`
- Branch: `feature/clouds3n/genai-model-connections`
- Repository remains unborn: no commits, and project files remain untracked.
- No Tron files, Git branches, commits or remote state were changed.
- Validation environment: macOS arm64, Rust/Cargo 1.95.0.

## Acceptance map

| Required area | Implemented behavior | Executable evidence |
| --- | --- | --- |
| Messages and results | Ordered typed content, structured tool results, generation options, offline JSON Schema validation | `agent-framework/tests/execution_contracts.rs`; `agent-model-genai/tests/protocol.rs` |
| Shared model connections and strategies | One provider/authentication per connection, multiple descriptors; fixed/stage/failover selection; current history visible to custom routing | `agent-framework/tests/model_connections.rs`; `agent-framework/tests/model_and_tool_controls.rs`; `agent-state-sqlite/tests/workflow.rs` |
| Extension contracts | Separate lifecycle hook contributions and ordered run/node/model/tool interceptors; bounded hook failure isolation, immutable run/tool continuations, per-step context contributions and terminal contract checks | `agent-framework/tests/hooks_and_interceptors.rs`; `agent-framework/tests/execution_contracts.rs`; `agent-framework/tests/host_contract.rs` |
| Token estimation and admission | Connection-level model-aware estimator throughout built-in planning and final admission; atomic reservation, conservative unknown usage, JSON-output capability check | `agent-framework/tests/model_and_tool_controls.rs`; `agent-framework/tests/execution_contracts.rs` |
| Resource lifecycle | Async reverse-order shutdown, sync Drop fallback, bounded workflow lease release, installation rollback; execution and cleanup errors retained together; cancelled close waiter cannot abandon cleanup | `agent-plugin-kernel/tests/async_shutdown.rs`; `agent-plugin-kernel/tests/container.rs`; `agent-framework/tests/async_lifecycle.rs`; release-fault tests in `agent-state-sqlite/tests/workflow.rs` |
| Tools and run control | Schema before effects; bounded opt-in ReAct parallelism; cloneable RunControl independent of result waiting, owner-drop cancellation, tool revocation at dispatch; post-effect middleware failure remains indeterminate | `agent-framework/tests/runtime_control.rs`; `agent-framework/tests/model_and_tool_controls.rs`; `agent-state-sqlite/tests/workflow.rs` |
| Independent execution scenarios | Simple chat, tools, configurable stages, input, approval, nested child Agents through real invokers | `agent-framework/tests/host_contract.rs`; `agent-framework/tests/delegation.rs`; `agent-state-sqlite/tests/workflow.rs` |
| Child execution | Root/parent identity, inherited limits, upfront allowance; rejected/interrupted admission refunds local budget/count; confirmed reservations corrected after rejected dispatch; unknown writes remain conservative | `agent-framework/tests/delegation.rs`; admission journal fault tests in `agent-framework/src/tests.rs`; durable child budget/count test in `agent-state-sqlite/tests/workflow.rs` |
| Persistence | Atomic checkpoint/transcript/journal; session-head predecessor checks, CAS revisions; stable fencing identity across renewals with authoritative store expiry; bounded worker admission, WAL and FULL synchronous mode | `agent-state-sqlite/tests/store.rs`; `agent-state-sqlite/src/lib.rs` cancellation test |
| Persistent questions and answers | Shared InteractionStore / InteractionManager; structured AskUserTool, append-only answer messages; exact ReAct approval; AG-UI interrupt and next-execution resume across restart | `agent-storage-sqlite/tests/interactions.rs`; `tests/stores.rs` |
| SQLite default storage | One explicit file for six storage roles, immutable binary artifacts, no implicit in-memory fallback, reverse-order owned connection cleanup | `agent-storage-sqlite/tests/stores.rs`; `tests/interactions.rs` |
| Native SQLite extensions | Independent sqlite-extensions package, target/ABI resource selection, pinned SHA-256, atomic content-checked extraction, connection-local loading and explicit capability reports | `sqlite-extensions/src/lib.rs`; all-feature `agent-storage-sqlite/tests/stores.rs` |
| Durable execution and approval | Intent and per-call result journal; exact approved descriptor/arguments; typed answers; explicit Record/Retry reconciliation; terminal replay without tools | `agent-state-sqlite/tests/workflow.rs`; `agent-state-sqlite/tests/crash_recovery.rs` |
| DAG execution and plugins | petgraph 0.8.3 topology validation; contribution-based node registry/configuration schemas; bounded Tokio JoinSet; selected/all joins and skip propagation; atomic node result/route persistence; explicit node retry/record; embedded ReAct checkpoints | 16 tests in `agent-state-sqlite/tests/dag.rs`; 2 additional DAG process-kill scenarios in `tests/crash_recovery.rs`; offline `examples/dag.rs` |
| Shared DAG controls | ReAct nodes share token, step and tool-count admission; run-wide tool concurrency and exclusive unsafe-tool dispatch; independently addressed approvals; uncertain storage stops progression | Parallel budget, approval, safety, limits and injected-storage-fault cases in `agent-state-sqlite/tests/dag.rs` |
| Recovery failure boundaries | Real executor process kill, in-flight budget reservation, completed/pending tool siblings, simultaneous resume, definition drift after optimistic read, schema drift, policy failure, SQL completion after await cancellation | `agent-state-sqlite/tests/crash_recovery.rs`; `agent-state-sqlite/tests/workflow.rs`; `agent-state-sqlite/src/lib.rs` |
| Event semantics | Distinct live execution-attempt and durable-journal scopes; serialized publication; workflow steering messages and events committed together before live publication, including final replies; bounded normal-event drain followed by a separately bounded terminal attempt per sink | `agent-framework/tests/execution_contracts.rs`; steering/replay/failure and acquired-checkpoint/event-scope tests in `agent-state-sqlite/tests/workflow.rs` |
| AG-UI output | Community ag-ui 0.3.0 event types and SSE; ordered bounded handoff independent of ordinary observer backlog; text/reasoning/tool/step/state/activity/custom events; distinct model/tool invocation identities, offloaded result references, tool-bound approval/denial across executor restart, final-response deduplication and serialized admission checks; explicit overflow/size/order errors | 22 SDK-client and lifecycle cases in `agent-agui/tests/protocol.rs`; offline `agent-agui/examples/sse.rs` |
| genai | Exact `=0.7.0-beta.21`, real local HTTP/SSE, shared auth, model discovery, tools, signed-thinking continuation, media input and generation options | `agent-model-genai/tests/protocol.rs` |
| MCP | Official rmcp 3.2.0 stdio and Streamable HTTP, bounded discovery/cursor detection, typed errors/results, protocol cancellation on cancel/drop, owned/shared close | `agent-tools-mcp/tests/transport.rs` launches actual SDK services over HTTP and in a child process |
| Conversation storage and windows | SQLite conversations, branch/generation identity, versioned atomic batches; independent context quotas and explicit layout; no source-history trimming | `agent-framework/tests/context_windows.rs`; `agent-memory/tests/stores.rs` |
| Long-term memory | Versioned/expiring records, explicit writes, bounded keyword search, fresh context retrieval, host-bound tool namespace | `agent-memory/tests/stores.rs`; `agent-memory/tests/components.rs` |
| Summary persistence | Model-aware packing, SQLite CAS store, range fingerprints, incremental reuse after reopen, branch/policy isolation | `agent-context/tests/compression.rs`; `summary_store.rs`; `agent-memory/tests/components.rs` |
| Node conversation records | Atomic node-state and interaction-batch recording, stable history across resume, explicit layout across all three loops | `agent-state-sqlite/tests/context_windows.rs` |
| Compression | Model-backed auxiliary calls use the same middleware/budget journal, silent summary deltas, preserved latest signed/tool turn, explicit failure on unusable summaries; raw history unchanged | `agent-context/tests/compression.rs` covers both ReAct and SQLite Workflow projections |
| Artifacts | Directory capability, canonical random IDs, atomic immutable publication, fsync, bounded byte ranges, cancellation, symlink rejection, combined content/structured offload, explicit result reference, post-publication/history-failure reconciliation | `agent-context/src/artifact.rs`; `agent-context/tests/artifacts.rs` |
| OpenTelemetry | Injected providers; run/node/model/tool spans, node-scoped model/tool links, persisted attempt identities across restore/retry, bounded redacted error events, duration/usage/error/cancellation metrics; inline assembly/failure/dispatch diagnostics while spans are alive; no payloads by default; providers remain host-owned | `agent-observability/tests/export.rs` uses actual SDK in-memory exporters |

## Final verification

- `cargo test --workspace --all-targets`: **232 standard tests passed**, plus
  **3 MCP protocol scenarios** and **4 forced-process-crash scenarios** from
  harness-free executables. No live provider credential is required.
- `cargo test --workspace --all-targets --all-features`: **234 standard tests passed**,
  including native extension queries and initialization of all five storage connections.
- `cargo fmt --all -- --check`: passed.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: passed.
- `cargo test --workspace --doc`: passed (no doctest examples).
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps`: passed.
- Validation used a per-command public crates.io source override because the configured mirror
  was unavailable. Local HTTP fixtures bypassed the system proxy via per-command NO_PROXY;
  global Cargo and proxy configuration were not changed.
- `cargo package -p sqlite-extensions --list --allow-dirty`: verified the package includes
  native resources, build-time checksums, provenance and licenses. Nothing was published.
- All source/document/manifest files passed a trailing-whitespace scan. Because
  the repository is unborn, `git diff --check` alone does not cover these files.
- Offline `standalone` is run twice against the same database with 19 + 23 and -11 + 4;
  each run uses the current turn and returns 42 and -7 respectively.
- `durable_approval` is exercised as separate start/answer processes against one
  SQLite file, including repeated answer replay.
- `shared_connection` compiles as a one-run, two-stage workflow with separately
  selected models. It and MCP `discover` access real services only when manually run.
- `dag` runs offline with both signs against the same SQLite session; results are
  7 for -7 and 14 for 7, with the other branch explicitly Skipped.

## Explicit behavior and validation limits

- SQLite implementations live only in plugin crates; the independent sqlite-extensions package handles optional native extensions. Framework owns the storage and context abstractions.
- Native extension execution was verified on macOS arm64 only. Other resources were checksum-verified; OS/CPU/ABI mapping tests do not establish runtime compatibility on those systems. Default storage does not require either extension.
- Long-term memory defaults to keyword matching, with no automatic fact extraction, vector search or retrieval-content cache.
- Artifact retention defaults to indefinite storage. The host collects authoritative references and schedules deletion only after quiescing writes and reconciling unknown results; no background GC is implied.
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
- Existing WorkflowLoop checkpoints accept the additive optional conversation generation field. New sequential
  definitions can compile to DAG chains; there is no implicit checkpoint migration.
- A Started external call is not automatically repeated. Exactly-once effects
  require external idempotency or explicit host reconciliation.
- `run_child` creates a new execution and rejects implicit reparenting/resume of
  existing durable child executions. Cross-process child orchestration remains
  an explicit host responsibility, not a hidden in-process guarantee.
- The reference compressor uses conservative estimates; tokenizer-specific
  custom reducers can implement ContextReducer. A summary can lose semantic
  detail, so raw history remains available and is never replaced by the summary.
- Artifact offload persists the complete content and structured result, retaining status,
  interaction effects and an explicit result reference inline. The host owns artifact authorization and retention.
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
- Explicitly shared SQLite, MCP and telemetry infrastructure is host-owned. The SQLite bundle
  owns and closes the connections it creates. This work does
  not claim account-level subscription billing, cross-node quota coordination,
  arbitrary plugin sandboxing, lowest-Rust-version certification or load certification.

The execution invariants and acceptance scope are retained in [execution-contracts.md](execution-contracts.md). Current layers and defaults are described in [architecture.md](architecture.md), with the extension inventory in [plugins.md](plugins.md).
