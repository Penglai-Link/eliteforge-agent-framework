# Framework memory

`eliteforge-agent-memory` owns long-term memory access, short-term block policy,
selection, source reads, projection writes and compaction. Context aggregation
uses the Framework `DefaultContextAssembler`, rather than a second memory assembler. The host
owns authorization, product records, storage transactions and UI metadata.

The public short-term boundary is `MemoryScope` + `MemoryRevision`,
`ShortTermMemorySource<T>`, `ShortTermMemoryProjectionStore<T>` and
`ShortTermMemoryEngine` compatibility projection API. New multi-provider context
uses `MemoryContextContributor` and `ContextLayout`. A source is bound by the host before reading; returned
records cannot change its namespace/session. Projection stores must compare
scope, generation and revision atomically with writes. A projection is derived
memory and never replaces the source transcript.

`ContextBlockBudgets::plan` reserves model output before allocating independent
caps for seed instructions, checkpoint, related-session context, working state
and tools; history uses the actual remainder. `assess_request`,
`remaining_history_budget` and `ratio_tokens` provide consistent control
accounting. `estimate_serialized_tokens` conservatively counts all serialized
bytes plus framing; provider-specific token estimators can replace it at the
host mapping boundary. Assembly preserves trusted seed messages and history,
bounds optional blocks under their actual estimates and reports each block. Disabled
flags apply only to optional blocks: seed instructions and current history always
survive intact and remain counted for final model admission.

`select_history` retains the complete latest user turn, the pinned current run
and matching tool IDs, including parallel batches. A narrow retention quota
never splits that required tail. Earlier complete turns are retained only when
they fit. An oversized required tail fails final model admission rather than
silently removing instructions, current input, tools or attachments.

`MemoryCompactor` delegates to the existing context `ModelCompressor`. It owns
the summary workflow and uses `ChatExecutor::invoke_auxiliary` for every call.
It reuses bounded whole-turn segmentation, call/input/output limits, JSON
summary validation, untrusted data serialization, cancellation and optional
`SummaryStore` revision/generation/fingerprint/CAS. `compact_history_with_usage`
returns both conservative run-ledger `usage` and optional aggregate
`provider_usage`. Zero/zero usage is unknown; if any segment does not report
usage, provider usage is unavailable while ledger reservations remain charged.
Reported totals must also equal this execution's ledger delta; failed internal
retry attempts leave the complete actual audit unavailable.
The result's public chat-response bridge preserves this distinction through SDK jobs. `MemoryCompactionMetadata`
passes operator notes and retained-tail boundaries as data. Native `ContextReducer`
integration and the core contributor/assembler/selection types are publicly
available from this plugin; no second compressor is introduced.

Regression commands:

```sh
cargo test -p eliteforge-agent-memory --test short_term --test compaction --test provider_context
cargo test -p eliteforge-agent-context --test compression
```

The tests cover block ordering and budgets, full latest turns, exact parallel
tool pairing, scope isolation, projection updates and stale writes, segmented
summary calls, invalid schemas/tools/byte limits, provider failure, call caps,
cancellation, timeout and close. Persistent summary reuse and CAS are exercised
by the existing context compression regressions.

## Provider layout and pressure policy

`MemoryContextContributor` exposes one authorized data source per block. The
host prepares an in-memory scoped snapshot; `MemoryContextSource::read` must not
perform blocking IO. An optional asynchronous `MemoryContextRevisionGuard`
checks the live source before and after semantic reduction. Instructions and
complete protocol history use the assembler builtins, so ordinary data cannot
inject system messages or native tool calls/results. Every Instruction-trust
block is immutable and bypasses its configured reducer entirely.

The default assembler fetches each source once, measures tools/options first,
then coordinates reductions. `ContextLayout` determines final message order
and reports one entry per block. `ContextBlockConfig::compress_at` separates
trigger and target percentages (basis points in the SDK, e.g. 8000 = 80%);
`with_min_compression_gain` rejects rewrites with insufficient measured savings.
The target is soft; each allocated token/message cap and final aggregate cap
remain hard. A block crossing its trigger receives one directive. A global
trigger gives every configured compressible block a Global directive, including
blocks whose strategy returns NoGain. Single/global triggers never invoke the
same strategy twice within one assembly. Missing or unsuccessful capacity
recovery yields an explicit error before the answer-model request.

`BorrowUnusedMemoryBudget` gives fixed data its actual capped cost and lends the
remaining capacity to builtin history. Percent caps are based on input capacity
after tool/options protocol; the caller already reserves answer-model output.
Fixed-block pressure is relative to its configured cap; history pressure is
relative to its actual borrowed quota. Reduction happens before history budget
reallocation without changing prompt order. Reports expose original/final token
cost, final message count, final allocation, the temporary reduction quota, and
Block/Global/NoGain outcomes. Final source-aware allocations are revalidated
per block, including custom policies that shrink a quota late.

Strategies are distinct: `SelectContextEntries` selects whole data records;
`BoundMemoryData` performs bounded lossy field trimming using a host DTO mapper;
`MemorySemanticDataReducer` summarizes all data records through the existing
`ModelCompressor`. The latter appends an internal sentinel only for compressor
boundary handling; that sentinel never enters final context. History continues
to protect the complete latest real user/tool turn. Async source, strategy,
estimator and budget policy work stays under the same run cancellation/deadline.

## Cache stability, actual usage and cost

Semantic summaries reuse the existing `SummaryStore`: scope, block identity,
generation, policy/model/quota digest, source revision and covered-prefix
fingerprint all participate. Stable sources reuse their committed summary;
a stable generation can summarize appended records without replaying the old
covered prefix. Corrected prefixes, changed policy or another scope invalidate
reuse. A host may deliberately change a data generation when its complete
product document changes; this is distinct from the historical checkpoint +
recent raw-tail path. NoGain restores original context, while a valid cached
candidate can avoid paying for the same unsuccessful rewrite again. A summary
for a stale generation cannot overwrite or become the projection for a newer
generation. Live revision rejection retains the paid auxiliary audit. Pending
CAS futures obey cancellation/deadline before dispatch, and the host closes its
SummaryStore. A SQLite transaction already dispatched may finish atomically
after cancellation; its generation key and version CAS still prevent it from
overwriting a newer generation or a concurrent winning version. Cancellation
does not claim to roll back a database transaction that already committed.

`MemoryCompactionResult::usage` is the conservative shared-run ledger delta.
`provider_usage` is actual only when every segment reports it and its aggregate
matches that delta. `provider_metrics` carries optional cached-input/reasoning
counts; one missing segment field keeps that field unknown. Cache reuse with no
model calls reports zero charge and no new provider usage, avoiding duplicate
actual statistics. These details feed the host's existing usage audit/UI chain.

Capacity safety is the admission rule. Trigger/target separation and minimum
gain provide hysteresis and avoid unproductive context rewrites; persisted
summaries reduce unnecessary auxiliary requests. Prices are host configuration,
not hardcoded SDK constants. Neither price declarations nor token reductions
alone prove monetary savings: auxiliary input/output, later cached and uncached
input, output, and retrieval costs all matter. Without provider cache metrics,
prices or future request counts the cost remains unknown. This implementation
does not claim an automatic fee-optimal choice or guaranteed cache hits; a host
can supply a pricing-aware strategy through the same reducer boundary without
creating another assembler or model executor.
