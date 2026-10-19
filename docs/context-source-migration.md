# Implemented unified context architecture

The inventory was a historical read-only snapshot. Its proposed parallel instruction premerge/deprecation route was superseded by the user's instruction to simplify the entire architecture.

## One read interface and one assembler

All six logical sources use `ContextContributor::contribute`: SDK builtin instructions, SDK builtin history, application, checkpoint, family, and working memory. `ContextSourceKind` describes protected Instruction, protected History, or Data semantics. `DefaultContextAssembler` constructs its two SDK adapters and reads every layout source through the same cancellation/deadline/source-timeout guard. Each resulting block follows the same allocation and reducer lifecycle. History retains its tool protocol and immutable projection checks; System instructions use `KeepContext` and are never compressed or demoted to data.

Instruction addons implement the same contract and return `ContextBlock::instructions(id, messages)`. The builtin instruction adapter composes stage instructions and installed addons exactly once per assembly, within the instruction query quota. Addon identities and source versions appear under the builtin block's `sources`; builtin `kind` reports Instruction. Addons do not create extra data slots, and their token usage is counted in the builtin instruction aggregate once. Data sources remain independently allocated layout blocks. An external History-kind source or reserved builtin source ID is rejected.

The registry places instruction contributors in `AgentServices.context_sources`. `ScopedAgentServices` already follows the native run through custom preparation, so `DefaultContextAssembler` reads the registry inside its protected instruction adapter. There is no premerge in `ModelInvoker` or the workflow model loop. A custom preparing assembler can delegate to this one default assembler, as Tron does; separate registered Data/layout/reducer plugins remain rejected when a custom assembler would ignore them.

## Native breaking changes

| Removed native API | Replacement |
| --- | --- |
| `ContextProvider::context(RunContext) -> Vec<Message>` | `ContextContributor::{kind,block_id,contribute}(ContextInput) -> ContextBlock` |
| `ContextService` | `ContextContributorService` |
| `ContextPlugin` / `ContextProviderPlugin` | `ContextContributorPlugin` |
| `FixedContext` | `FixedInstructions::new(id, text)` / `FixedInstructions::messages(id, messages)` |
| `AgentServices.context` | `AgentServices.context_sources` for installed Instruction sources |
| `TronHarness::new(context, tools, catalog)` | `TronHarness::new(tools, catalog)` and optional `.with_context_source(source)` |

All known native implementations, skills registration, macro fixtures, examples, tests, guides, and public glob exports are migrated. Capability installation supplies an instruction source's declared plugin ID, while retaining the same underlying shared native implementation. Downstream code outside the inspected repositories will need the same Rust API migration; no zero-impact compatibility claim is made.

## Remote version-1 boundary

The existing manifest contract `framework.context-provider` version 1 and operation `context` returning `Vec<Message>` remain accepted in the remote capability runtime. That boundary adapts the old response into an Instruction-kind `ContextContributor` and `ContextBlock`. The ordinary remote contributor contract remains `framework.context-contributor` / `contribute` returning a block. No deprecated native provider/service/plugin contract is retained, and no unversioned wire schema change was invented.

`PersonAiContextProvider` remains a separate credential/account notification API. Tron's `ContextProviderPolicy` and `ContextProvidersConfig` remain product source configuration DTOs.

## Meaningful verification added

`eliteforge-agent-framework/tests/unified_context_sources.rs` runs real model paths for six logical sources and report kinds/provenance, skills catalog composition exactly once with a custom preparing assembler, cancellation before model admission, and instruction quota failure with provenance diagnostics. Existing native capability, skills, context window, protected instruction, and remote sidecar tests are migrated as regression coverage. The parent task runs serialized Cargo checks; result files are separate from the immutable accepted baseline evidence.

## Product shared seed allocation

Tron `context_providers.rs` grants both builtin instructions and application data the configured shared seed pool as their read allowance. Its private `context_budget.rs::SharedSeedBudget` uses the SDK's existing `ContextBudgetPolicy::allocate_sources` hook after those single reads: the complete protected instruction cost (prefix, stage, and addons) receives exact allocation; application data receives the remaining seed pool; `BorrowUnusedMemoryBudget` lends unused total capacity to history. There is no host preload of addon sources, no second aggregator, and no raised call ceiling. The configured 50% input ratio and 16,000-token hard seed cap remain binding. Over-budget instructions fail closed.

New Tron `context_seed_budget_tests.rs` installs the genuine `SkillsPlugin` endpoint into a real `FrameworkHost` with `TronContext`, explicit fixture tool authority, and a native-only fixture tool provider. It checks both ratio-limited and hard-cap windows, one skills catalog read/injection, source provenance, exact instruction allocation, history remainder, and failure before model admission for oversized metadata. A focused allocator test checks nonempty application data sharing and the protected over-budget guard. Parent-run compilation and test results remain pending.

Compatibility limit: a remote version-1 `framework.context-contributor` is Data by declaration. Older custom layouts which returned Instruction trust through this ordinary Data contract must migrate to the declared instruction boundary; the legacy `framework.context-provider` wire contract itself remains accepted.
