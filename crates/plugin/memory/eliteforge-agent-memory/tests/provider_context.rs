use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_memory::*;
use eliteforge_agent_official_plugin_storage_local::{SqliteSummaryStore, SummaryStoreOptions};
use serde_json::json;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use std::time::Duration;

#[derive(Clone, Default)]
struct Provider {
    requests: Arc<Mutex<Vec<ChatRequest>>>,
    change: Option<Arc<AtomicU64>>,
}
#[async_trait]
impl ChatModel for Provider {
    async fn generate(
        &self,
        model: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        self.requests.lock().unwrap().push(request);
        if model == "summary" {
            if let Some(revision) = &self.change {
                revision.fetch_add(1, Ordering::SeqCst);
            }
            Ok(ChatResponse {
                content: json!({"summary":"retained contextual facts"})
                    .to_string()
                    .into(),
                usage: Usage {
                    input_tokens: 12,
                    output_tokens: 6,
                },
                ..Default::default()
            }
            .with_provider_metrics(ProviderMetrics {
                cached_input_tokens: Some(5),
                reasoning_tokens: Some(2),
            }))
        } else {
            Ok(ChatResponse::text("answer"))
        }
    }
}
struct Workflow {
    assembler: Arc<DefaultContextAssembler>,
    instructions: Vec<Message>,
    history: Vec<Message>,
    dispatch: bool,
}
#[async_trait]
impl ModelWorkflow for Workflow {
    async fn run(
        &self,
        run: &RunContext,
        models: &dyn ChatExecutor,
        limits: &RunLimits,
        model: &ModelRef,
    ) -> Result<ChatResponse, AgentError> {
        let snapshot = ConversationSnapshot::new(
            ConversationKey::new(&run.session_id),
            "history-generation",
            self.history.clone(),
        );
        let estimator = models.catalog().token_estimator(model).unwrap();
        let options = GenerationOptions::default();
        let tools = vec![ToolDescriptor::new(
            "lookup",
            "lookup",
            "authorized lookup",
            json!({"type":"object","properties":{"query":{"type":"string"}}}),
        )];
        let window = self
            .assembler
            .assemble(&ContextAssemblyInput {
                stage: "provider-test",
                attempt: 1,
                step: 0,
                model,
                budget: ContextBudget {
                    max_tokens: 8000,
                    max_messages: 64,
                },
                estimator,
                history: &snapshot,
                instructions: &self.instructions,
                tools: &tools,
                options: &options,
                run,
                models,
                limits,
                overflow_recoveries: 0,
            })
            .await?;
        if self.dispatch {
            models
                .invoke_auxiliary(
                    model,
                    ChatRequest {
                        messages: window.messages.clone(),
                        tools,
                        options,
                        extensions: run.extensions.clone(),
                    },
                    "final-wire",
                    limits,
                    run,
                )
                .await?;
        }
        Ok(ChatResponse {
            structured: Some(serde_json::to_value(window).unwrap()),
            ..Default::default()
        })
    }
}
fn fixture(
    assembler: DefaultContextAssembler,
    provider: Provider,
    instructions: Vec<Message>,
    dispatch: bool,
) -> (ModelClient, Arc<dyn ModelWorkflow>, RunLimits) {
    let workflow = Arc::new(Workflow {
        assembler: Arc::new(assembler),
        instructions,
        history: vec![Message::user("question")],
        dispatch,
    });
    let client = ModelClient::new(ModelRuntimeUpdate::new(
        "memory",
        1,
        vec![ModelConnection::new(
            "memory",
            Arc::new(provider),
            vec![
                ModelDescriptor::new("main"),
                ModelDescriptor::new("summary"),
            ],
        )],
    ))
    .unwrap();
    let limits = RunLimits {
        max_model_retries: 0,
        max_context_tokens: 16000,
        max_model_output_tokens: 512,
        max_total_tokens: 100000,
        max_model_calls_per_step: Some(16),
        ..Default::default()
    };
    (client, workflow, limits)
}
async fn execute(
    (client, workflow, limits): (ModelClient, Arc<dyn ModelWorkflow>, RunLimits),
) -> Result<ContextWindow, AgentError> {
    let result = client
        .workflow_observed(
            &ModelRef::new("memory", "main"),
            workflow,
            &limits,
            "provider-test",
        )
        .await
        .into_result();
    client.close().await;
    result.map(|response| serde_json::from_value(response.structured.unwrap()).unwrap())
}
fn scope() -> MemoryScope {
    MemoryScope::new("workspace/user-a", "session-a").unwrap()
}
fn revision(value: u64) -> MemoryRevision {
    MemoryRevision {
        scope: scope(),
        generation: "stable-data-generation".into(),
        revision: value,
    }
}
fn source(id: &str, texts: Vec<String>, value: u64) -> Arc<ScopedMemoryContext> {
    Arc::new(ScopedMemoryContext {
        revision: revision(value),
        block: ContextBlock {
            id: id.into(),
            messages: texts.into_iter().map(Message::user).collect(),
            trust: ContextTrust::Data,
            source_version: None,
        },
    })
}
fn contributor(id: &str, source: Arc<ScopedMemoryContext>) -> Arc<dyn ContextContributor> {
    Arc::new(MemoryContextContributor::new(id, scope(), source).unwrap())
}
fn config(id: &str, tokens: usize) -> ContextBlockConfig {
    ContextBlockConfig::new(id, BlockBudget::tokens(tokens))
}

#[tokio::test]
async fn actual_layout_changes_wire_order_and_independent_reports_without_duplicate_injection() {
    for order in [["a", "b"], ["b", "a"]] {
        let a = source("a", vec!["data A".into()], 1);
        let b = source("b", vec!["data B".into()], 1);
        let assembler = DefaultContextAssembler::new(
            ContextLayout(vec![
                config(INSTRUCTIONS_BLOCK, 2000),
                config(order[0], 1000),
                config(order[1], 1000),
                config(HISTORY_BLOCK, 8000),
            ]),
            vec![contributor("b", b), contributor("a", a)],
        )
        .unwrap()
        .with_policy(Arc::new(BorrowUnusedMemoryBudget));
        let provider = Provider::default();
        let window = execute(fixture(
            assembler,
            provider.clone(),
            vec![Message::system("trusted prefix")],
            true,
        ))
        .await
        .unwrap();
        let requests = provider.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 1);
        let expected = vec![
            "trusted prefix",
            if order[0] == "a" { "data A" } else { "data B" },
            if order[1] == "a" { "data A" } else { "data B" },
            "question",
        ];
        assert_eq!(
            requests[0]
                .messages
                .iter()
                .map(|message| message.content.text())
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            window
                .report
                .blocks
                .iter()
                .map(|report| report.id.as_str())
                .collect::<Vec<_>>(),
            vec![INSTRUCTIONS_BLOCK, order[0], order[1], HISTORY_BLOCK]
        );
        assert!(window.report.protocol_tokens > 0);
        assert!(
            window
                .report
                .blocks
                .iter()
                .all(|report| report.final_tokens <= report.allocated_tokens)
        );
        assert!(
            window
                .report
                .blocks
                .iter()
                .map(|report| report.allocated_tokens)
                .sum::<usize>()
                <= 8000 - window.report.protocol_tokens
        );
        assert!(window.report.blocks[3].allocated_tokens > 4000);
    }
}
struct Spy {
    active: Arc<AtomicUsize>,
    invocations: Arc<AtomicUsize>,
    output: Option<String>,
}
#[async_trait]
impl ContextReducer for Spy {
    async fn reduce(&self, input: &ContextReductionInput<'_>) -> Result<Vec<Message>, AgentError> {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        if input.compression.is_some() {
            self.active.fetch_add(1, Ordering::SeqCst);
            if let Some(output) = &self.output {
                return Ok(vec![Message::user(output.clone())]);
            }
        }
        Ok(input.block.messages.clone())
    }
}
struct CountSource {
    id: &'static str,
    calls: Arc<AtomicUsize>,
    body: String,
}
#[async_trait]
impl ContextContributor for CountSource {
    fn block_id(&self) -> &str {
        self.id
    }
    async fn contribute(&self, _: &ContextInput<'_>) -> Result<ContextBlock, AgentError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ContextBlock::data(self.id, self.body.clone()))
    }
}
#[tokio::test]
async fn block_and_global_pressure_coordinate_once_and_never_call_instruction_strategy() {
    for global in [false, true] {
        let mut counters = vec![];
        let mut blocks = vec![];
        for id in [INSTRUCTIONS_BLOCK, "a", "b", HISTORY_BLOCK] {
            let active = Arc::new(AtomicUsize::new(0));
            let calls = Arc::new(AtomicUsize::new(0));
            let mut block =
                config(id, if id == HISTORY_BLOCK { 2000 } else { 1000 }).reduce_with(Spy {
                    active: active.clone(),
                    invocations: calls.clone(),
                    output: if ["a", "b"].contains(&id) {
                        Some("short facts".into())
                    } else {
                        None
                    },
                });
            if id != INSTRUCTIONS_BLOCK {
                block = block.compress_at(8000, 5000);
            }
            counters.push((active, calls));
            blocks.push(block);
        }
        let reads = Arc::new(AtomicUsize::new(0));
        let sources: Vec<Arc<dyn ContextContributor>> = vec![
            Arc::new(CountSource {
                id: "a",
                calls: reads.clone(),
                body: "A".repeat(900),
            }),
            Arc::new(CountSource {
                id: "b",
                calls: reads.clone(),
                body: "B".repeat(100),
            }),
        ];
        let assembler = DefaultContextAssembler::new(ContextLayout(blocks), sources)
            .unwrap()
            .with_global_compression(Some(if global { 1000 } else { 8000 }), false)
            .unwrap();
        let window = execute(fixture(
            assembler,
            Provider::default(),
            vec![
                Message::system("trusted prefix").with_instruction_origin(InstructionOrigin::Agent),
            ],
            false,
        ))
        .await
        .unwrap();
        assert_eq!(reads.load(Ordering::SeqCst), 2);
        assert_eq!(counters[0].1.load(Ordering::SeqCst), 0);
        assert_eq!(counters[1].0.load(Ordering::SeqCst), 1);
        assert_eq!(counters[2].0.load(Ordering::SeqCst), usize::from(global));
        assert_eq!(counters[3].0.load(Ordering::SeqCst), usize::from(global));
        assert!(
            counters[1..]
                .iter()
                .all(|(_, calls)| calls.load(Ordering::SeqCst) == 1)
        );
        assert_eq!(
            window.messages[0],
            Message::system("trusted prefix").with_instruction_origin(InstructionOrigin::Agent)
        );
        assert_eq!(window.messages.last().unwrap(), &Message::user("question"));
        assert_eq!(
            window.report.blocks[1].compression_trigger,
            if global {
                ContextCompressionTrigger::Global
            } else {
                ContextCompressionTrigger::Block
            }
        );
    }
}
#[tokio::test]
async fn same_or_larger_token_rewrites_are_no_gain_and_keep_original_stable_data() {
    for length in [100, 200] {
        let active = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let assembler = DefaultContextAssembler::new(
            ContextLayout(vec![
                config(INSTRUCTIONS_BLOCK, 100),
                config("data", 500).compress_at(1000, 500).reduce_with(Spy {
                    active,
                    invocations: calls,
                    output: Some("Y".repeat(length)),
                }),
                config(HISTORY_BLOCK, 1000),
            ]),
            vec![contributor(
                "data",
                source("data", vec!["X".repeat(100)], 1),
            )],
        )
        .unwrap();
        let window = execute(fixture(assembler, Provider::default(), vec![], false))
            .await
            .unwrap();
        assert_eq!(window.messages[0], Message::user("X".repeat(100)));
        assert!(matches!(
            window.report.blocks[1].outcome,
            ContextBlockOutcome::NoGain
        ));
    }
}
#[tokio::test]
async fn custom_instruction_provider_is_immutable_or_rejects_compression_before_any_model() {
    for compression in [false, true] {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut block = config(INSTRUCTIONS_BLOCK, 1000).reduce_with(Spy {
            active: Default::default(),
            invocations: calls.clone(),
            output: Some("rewritten".into()),
        });
        if compression {
            block = block.compress_at(8000, 5000);
        }
        let source = FixedContextContributor(ContextBlock {
            id: "instruction".into(),
            messages: vec![Message::system("immutable custom instructions")],
            trust: ContextTrust::Instruction,
            source_version: None,
        });
        let assembler = DefaultContextAssembler::new(
            ContextLayout(vec![block, config(HISTORY_BLOCK, 1000)]),
            vec![Arc::new(source)],
        );
        if compression {
            assert!(assembler.is_err());
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            continue;
        }
        let provider = Provider::default();
        let result = execute(fixture(assembler.unwrap(), provider.clone(), vec![], true)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        {
            assert_eq!(
                result.unwrap().messages[0],
                Message::system("immutable custom instructions")
                    .with_instruction_origin(InstructionOrigin::Plugin)
            );
        }
    }
}
struct ShrinkFinal(AtomicUsize);
#[async_trait]
impl ContextBudgetPolicy for ShrinkFinal {
    async fn allocate(
        &self,
        blocks: &[ContextBlockConfig],
        available: ContextBudget,
    ) -> Result<Vec<ContextBudget>, AgentError> {
        IndependentBlockBudget.allocate(blocks, available).await
    }
    async fn allocate_sources(
        &self,
        blocks: &[ContextBlockConfig],
        available: ContextBudget,
        _: &[usize],
    ) -> Result<Vec<ContextBudget>, AgentError> {
        let count = self.0.fetch_add(1, Ordering::SeqCst);
        let mut budgets = self.allocate(blocks, available).await?;
        if count >= 2 {
            budgets[1].max_tokens = 0;
            budgets[1].max_messages = 0;
        }
        Ok(budgets)
    }
}
#[tokio::test]
async fn final_source_aware_policy_cannot_shrink_a_reported_quota_without_rejecting_output() {
    let assembler = DefaultContextAssembler::new(
        ContextLayout(vec![
            config(INSTRUCTIONS_BLOCK, 100),
            config("data", 500),
            config(HISTORY_BLOCK, 1000),
        ]),
        vec![contributor(
            "data",
            source("data", vec!["important data".into()], 1),
        )],
    )
    .unwrap()
    .with_policy(Arc::new(ShrinkFinal(AtomicUsize::new(0))));
    let provider = Provider::default();
    let error = execute(fixture(assembler, provider.clone(), vec![], true))
        .await
        .err()
        .expect("expected context failure");
    if let AgentError::ContextAssembly { report, .. } = error {
        assert_eq!(report.blocks[1].allocated_tokens, 0);
        assert!(matches!(
            report.blocks[1].outcome,
            ContextBlockOutcome::Failed { .. }
        ));
    } else {
        panic!("missing block failure report");
    }
    assert!(provider.requests.lock().unwrap().is_empty());
}
struct SlowPolicy(Arc<AtomicUsize>);
#[async_trait]
impl ContextBudgetPolicy for SlowPolicy {
    async fn allocate(
        &self,
        _: &[ContextBlockConfig],
        _: ContextBudget,
    ) -> Result<Vec<ContextBudget>, AgentError> {
        self.0.store(1, Ordering::SeqCst);
        std::future::pending().await
    }
}
#[tokio::test]
async fn slow_budget_policy_obeys_cancellation_and_run_deadline_without_provider_dispatch() {
    for cancel in [false, true] {
        let started = Arc::new(AtomicUsize::new(0));
        let assembler =
            DefaultContextAssembler::default().with_policy(Arc::new(SlowPolicy(started.clone())));
        let provider = Provider::default();
        let (client, workflow, mut limits) = fixture(assembler, provider.clone(), vec![], true);
        if !cancel {
            limits.run_timeout = Duration::from_millis(50);
        }
        let options = ModelInvocationOptions::for_call(&limits).unwrap();
        let cancel_token = options.cancellation.clone();
        let waiting_client = client.with_options(options);
        let waiting = tokio::spawn(async move {
            waiting_client
                .workflow_observed(
                    &ModelRef::new("memory", "main"),
                    workflow,
                    &limits,
                    "provider-test",
                )
                .await
                .into_result()
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while started.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        if cancel {
            cancel_token.cancel();
        }
        assert!(
            tokio::time::timeout(Duration::from_secs(2), waiting)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert!(provider.requests.lock().unwrap().is_empty());
        client.close().await;
    }
}
#[derive(Default)]
struct Audit(Mutex<Vec<MemoryCompactionResult>>);
#[async_trait]
impl MemoryReductionAuditSink for Audit {
    async fn record(
        &self,
        _: &RunContext,
        _: &ContextBlock,
        result: &MemoryCompactionResult,
    ) -> Result<(), AgentError> {
        self.0.lock().unwrap().push(result.clone());
        Ok(())
    }
}
fn semantic_assembler(
    data: Arc<ScopedMemoryContext>,
    store: Arc<dyn SummaryStore>,
    audit: Arc<Audit>,
    guard: Option<Arc<dyn MemoryContextRevisionGuard>>,
) -> DefaultContextAssembler {
    let mut reducer = MemorySemanticDataReducer::new(
        ModelRef::new("memory", "summary"),
        CompressionOptions {
            max_input_tokens: 6000,
            max_summary_bytes: 128,
            max_model_calls: 8,
        },
        data.revision.clone(),
    )
    .unwrap()
    .with_store(store, "semantic-data-v1")
    .unwrap()
    .with_audit_sink(audit);
    if let Some(guard) = guard {
        reducer = reducer.with_source_guard(guard);
    }
    DefaultContextAssembler::new(
        ContextLayout(vec![
            config(INSTRUCTIONS_BLOCK, 2000),
            config("data", 3000)
                .compress_at(8000, 5000)
                .reduce_with(reducer),
            config(HISTORY_BLOCK, 8000),
        ]),
        vec![Arc::new(
            MemoryContextContributor::new("data", data.revision.scope.clone(), data).unwrap(),
        )],
    )
    .unwrap()
    .with_policy(Arc::new(BorrowUnusedMemoryBudget))
    .with_global_compression(Some(8000), true)
    .unwrap()
}
#[tokio::test]
async fn semantic_summary_reuses_cache_appends_only_new_records_and_preserves_provider_metrics() {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(
        SqliteSummaryStore::open(
            temp.path().join("summary.sqlite"),
            SummaryStoreOptions::default(),
        )
        .await
        .unwrap(),
    );
    let audit = Arc::new(Audit::default());
    let provider = Provider::default();
    let older = "ORIGINAL SOURCE A ".repeat(100);
    for _ in 0..2 {
        let data = source("data", vec![older.clone()], 1);
        execute(fixture(
            semantic_assembler(data, store.clone(), audit.clone(), None),
            provider.clone(),
            vec![Message::system("stable instructions")],
            true,
        ))
        .await
        .unwrap();
    }
    let initial = provider
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request.options.output_schema.is_some())
        .count();
    assert_eq!(initial, 1);
    let data = source("data", vec![older, "APPENDED SOURCE B ".repeat(100)], 2);
    execute(fixture(
        semantic_assembler(data, store.clone(), audit.clone(), None),
        provider.clone(),
        vec![Message::system("stable instructions")],
        true,
    ))
    .await
    .unwrap();
    let requests = provider.requests.lock().unwrap().clone();
    let summaries = requests
        .iter()
        .filter(|request| request.options.output_schema.is_some())
        .collect::<Vec<_>>();
    assert_eq!(summaries.len(), 2);
    assert!(
        !serde_json::to_string(summaries[1])
            .unwrap()
            .contains("ORIGINAL SOURCE A")
    );
    assert!(
        serde_json::to_string(summaries[1])
            .unwrap()
            .contains("APPENDED SOURCE B")
    );
    drop(requests);
    let observations = audit.0.lock().unwrap().clone();
    assert_eq!(observations.len(), 3);
    assert_eq!(
        observations[0].provider_usage,
        Some(Usage {
            input_tokens: 12,
            output_tokens: 6
        })
    );
    assert_eq!(
        observations[0].provider_metrics,
        Some(ProviderMetrics {
            cached_input_tokens: Some(5),
            reasoning_tokens: Some(2)
        })
    );
    assert_eq!(observations[1].usage, Usage::default());
    assert_eq!(observations[1].provider_usage, None);
    assert_eq!(observations[1].provider_metrics, None);
    drop(observations);
    store.close().await.unwrap();
}
struct LiveGuard {
    current: Arc<AtomicU64>,
}
#[async_trait]
impl MemoryContextRevisionGuard for LiveGuard {
    async fn ensure_current(&self, expected: &MemoryRevision) -> Result<(), MemoryError> {
        expected.ensure_current(&MemoryRevision {
            revision: self.current.load(Ordering::SeqCst),
            ..expected.clone()
        })
    }
}
#[tokio::test]
async fn late_semantic_result_is_rejected_after_live_source_revision_changes() {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(
        SqliteSummaryStore::open(
            temp.path().join("summary.sqlite"),
            SummaryStoreOptions::default(),
        )
        .await
        .unwrap(),
    );
    let current = Arc::new(AtomicU64::new(1));
    let provider = Provider {
        change: Some(current.clone()),
        ..Default::default()
    };
    let audit = Arc::new(Audit::default());
    let data = source("data", vec!["guarded facts ".repeat(120)], 1);
    let error = execute(fixture(
        semantic_assembler(
            data,
            store.clone(),
            audit.clone(),
            Some(Arc::new(LiveGuard { current })),
        ),
        provider.clone(),
        vec![],
        true,
    ))
    .await
    .err()
    .expect("expected context failure");
    assert!(matches!(
        error.primary(),
        AgentError::LongTermMemory(MemoryError::Conflict)
    ));
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    assert_eq!(
        audit.0.lock().unwrap()[0].provider_usage,
        Some(Usage {
            input_tokens: 12,
            output_tokens: 6
        })
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn history_pressure_uses_borrowed_capacity_and_minimum_gain_preserves_source() {
    let active = Arc::new(AtomicUsize::new(0));
    let (client, _, limits) = fixture(
        DefaultContextAssembler::default(),
        Provider::default(),
        vec![],
        false,
    );
    let workflow: Arc<dyn ModelWorkflow> = Arc::new(Workflow {
        assembler: Arc::new(
            DefaultContextAssembler::new(
                ContextLayout(vec![
                    config(INSTRUCTIONS_BLOCK, 1000),
                    config("fixed", 4000),
                    config(HISTORY_BLOCK, 8000)
                        .compress_at(6000, 4000)
                        .reduce_with(Spy {
                            active: active.clone(),
                            invocations: Default::default(),
                            output: None,
                        }),
                ]),
                vec![contributor(
                    "fixed",
                    source("fixed", vec!["fixed ".repeat(550)], 1),
                )],
            )
            .unwrap()
            .with_policy(Arc::new(BorrowUnusedMemoryBudget)),
        ),
        instructions: vec![],
        history: vec![
            Message::user("history ".repeat(360)),
            Message::assistant("done"),
            Message::user("question"),
        ],
        dispatch: false,
    });
    let window = execute((client, workflow, limits)).await.unwrap();
    assert_eq!(active.load(Ordering::SeqCst), 1);
    assert_eq!(
        window.report.blocks[2].compression_trigger,
        ContextCompressionTrigger::Block
    );
    assert!(window.report.blocks[2].original_tokens.unwrap() < 8000 * 6000 / 10000);
    let assembler = DefaultContextAssembler::new(
        ContextLayout(vec![
            config(INSTRUCTIONS_BLOCK, 100),
            config("data", 500)
                .compress_at(1000, 500)
                .with_min_compression_gain(20)
                .reduce_with(Spy {
                    active: Default::default(),
                    invocations: Default::default(),
                    output: Some("Y".repeat(90)),
                }),
            config(HISTORY_BLOCK, 1000),
        ]),
        vec![contributor(
            "data",
            source("data", vec!["X".repeat(100)], 1),
        )],
    )
    .unwrap();
    let window = execute(fixture(assembler, Provider::default(), vec![], false))
        .await
        .unwrap();
    assert_eq!(window.messages[0], Message::user("X".repeat(100)));
    assert!(matches!(
        window.report.blocks[1].outcome,
        ContextBlockOutcome::NoGain
    ));
}

struct GatedStore {
    inner: Arc<SqliteSummaryStore>,
    entered: AtomicUsize,
    writes: AtomicUsize,
    gate: tokio::sync::Semaphore,
    key: Mutex<Option<SummaryKey>>,
}
#[async_trait]
impl SummaryStore for GatedStore {
    async fn load(&self, key: &SummaryKey) -> Result<Option<VersionedSummary>, ContextError> {
        self.inner.load(key).await
    }
    async fn compare_exchange(
        &self,
        key: &SummaryKey,
        expected: Option<&str>,
        value: &ConversationSummary,
    ) -> Result<bool, ContextError> {
        *self.key.lock().unwrap() = Some(key.clone());
        self.entered.store(1, Ordering::SeqCst);
        let _permit = self.gate.acquire().await.unwrap();
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.compare_exchange(key, expected, value).await
    }
    async fn delete(&self, key: &SummaryKey, expected: &str) -> Result<bool, ContextError> {
        self.inner.delete(key, expected).await
    }
}
#[tokio::test]
async fn cancellation_and_deadline_drop_pending_summary_cas_and_close_sqlite() {
    for cancel in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let inner = Arc::new(
            SqliteSummaryStore::open(
                temp.path().join("summary.sqlite"),
                SummaryStoreOptions::default(),
            )
            .await
            .unwrap(),
        );
        let store = Arc::new(GatedStore {
            inner: inner.clone(),
            entered: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
            gate: tokio::sync::Semaphore::new(0),
            key: Mutex::new(None),
        });
        let data = source("data", vec!["source for cancelled summary ".repeat(100)], 1);
        let provider = Provider::default();
        let (client, workflow, mut limits) = fixture(
            semantic_assembler(data, store.clone(), Arc::new(Audit::default()), None),
            provider.clone(),
            vec![],
            true,
        );
        if !cancel {
            limits.run_timeout = Duration::from_millis(100);
        }
        let options = ModelInvocationOptions::for_call(&limits).unwrap();
        let cancel_token = options.cancellation.clone();
        let waiting_client = client.with_options(options);
        let waiting = tokio::spawn(async move {
            waiting_client
                .workflow_observed(
                    &ModelRef::new("memory", "main"),
                    workflow,
                    &limits,
                    "provider-test",
                )
                .await
                .into_result()
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while store.entered.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        if cancel {
            cancel_token.cancel();
        }
        assert!(
            tokio::time::timeout(Duration::from_secs(2), waiting)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        store.gate.add_permits(1);
        tokio::task::yield_now().await;
        assert_eq!(store.writes.load(Ordering::SeqCst), 0);
        let key = store.key.lock().unwrap().clone().unwrap();
        assert!(inner.load(&key).await.unwrap().is_none());
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
        client.close().await;
        inner.close().await.unwrap();
    }
}
#[tokio::test]
async fn persistent_summary_reopens_and_source_correction_or_scope_change_invalidates_cache() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("summary.sqlite");
    let provider = Provider::default();
    let store = Arc::new(
        SqliteSummaryStore::open(&path, SummaryStoreOptions::default())
            .await
            .unwrap(),
    );
    execute(fixture(
        semantic_assembler(
            source("data", vec!["original scoped facts ".repeat(100)], 1),
            store.clone(),
            Arc::new(Audit::default()),
            None,
        ),
        provider.clone(),
        vec![],
        false,
    ))
    .await
    .unwrap();
    store.close().await.unwrap();
    let store = Arc::new(
        SqliteSummaryStore::open(&path, SummaryStoreOptions::default())
            .await
            .unwrap(),
    );
    execute(fixture(
        semantic_assembler(
            source("data", vec!["original scoped facts ".repeat(100)], 1),
            store.clone(),
            Arc::new(Audit::default()),
            None,
        ),
        provider.clone(),
        vec![],
        false,
    ))
    .await
    .unwrap();
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    execute(fixture(
        semantic_assembler(
            source("data", vec!["corrected scoped facts ".repeat(100)], 2),
            store.clone(),
            Arc::new(Audit::default()),
            None,
        ),
        provider.clone(),
        vec![],
        false,
    ))
    .await
    .unwrap();
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    let mut data = (*source("data", vec!["corrected scoped facts ".repeat(100)], 2)).clone();
    data.revision.scope = MemoryScope::new("workspace/user-b", "session-a").unwrap();
    let reducer = MemorySemanticDataReducer::new(
        ModelRef::new("memory", "summary"),
        CompressionOptions {
            max_input_tokens: 6000,
            max_summary_bytes: 128,
            max_model_calls: 8,
        },
        data.revision.clone(),
    )
    .unwrap()
    .with_store(store.clone(), "semantic-data-v1")
    .unwrap();
    let contributor =
        MemoryContextContributor::new("data", data.revision.scope.clone(), Arc::new(data)).unwrap();
    let assembler = DefaultContextAssembler::new(
        ContextLayout(vec![
            config(INSTRUCTIONS_BLOCK, 2000),
            config("data", 3000)
                .compress_at(8000, 5000)
                .reduce_with(reducer),
            config(HISTORY_BLOCK, 8000),
        ]),
        vec![Arc::new(contributor)],
    )
    .unwrap()
    .with_policy(Arc::new(BorrowUnusedMemoryBudget))
    .with_global_compression(Some(8000), true)
    .unwrap();
    execute(fixture(assembler, provider.clone(), vec![], false))
        .await
        .unwrap();
    assert_eq!(provider.requests.lock().unwrap().len(), 3);
    store.close().await.unwrap();
}

#[tokio::test]
async fn platform_semantic_summary_cache_is_distinct_from_same_named_namespace() {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(
        SqliteSummaryStore::open(
            temp.path().join("platform.sqlite"),
            SummaryStoreOptions::default(),
        )
        .await
        .unwrap(),
    );
    let provider = Provider::default();
    let named = MemoryScope::new("platform", "session-a").unwrap();
    let platform = MemoryScope::platform("session-a").unwrap();
    for scope in [named.clone(), platform, named] {
        let mut data = (*source("data", vec!["scoped historical facts ".repeat(100)], 1)).clone();
        data.revision.scope = scope;
        execute(fixture(
            semantic_assembler(
                Arc::new(data),
                store.clone(),
                Arc::new(Audit::default()),
                None,
            ),
            provider.clone(),
            vec![],
            false,
        ))
        .await
        .unwrap();
    }
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    store.close().await.unwrap();
}

#[test]
fn percentage_allocations_allow_partial_totals_without_filling_unallocated_capacity() {
    let shares = |a, b| std::collections::BTreeMap::from([("a".into(), a), ("b".into(), b)]);
    assert!(eliteforge_agent_memory::ContextShareAllocation::new(shares(5000, 5001)).is_err());
    assert!(eliteforge_agent_memory::ContextShareAllocation::new(shares(0, 5000)).is_err());
    for (a, b) in [(5000, 4999), (3333, 1667)] {
        let partial = eliteforge_agent_memory::ContextShareAllocation::new(shares(a, b)).unwrap();
        for budget in [1, 9, 128_001, 1_000_000, usize::MAX] {
            let result = partial.allocate(budget);
            let expected = (budget as u128 * u128::from(a + b) / 10_000) as usize;
            assert_eq!(result.values().sum::<usize>(), expected);
        }
    }
    let partial = eliteforge_agent_memory::ContextShareAllocation::new(shares(4000, 2000)).unwrap();
    assert_eq!(
        partial.allocate(9),
        std::collections::BTreeMap::from([("a".into(), 3), ("b".into(), 2)])
    );
    let allocation = eliteforge_agent_memory::ContextShareAllocation::new(shares(3333, 6667)).unwrap();
    for budget in [1, 128_001, 1_000_000] {
        let result = allocation.allocate(budget);
        assert_eq!(result.values().sum::<usize>(), budget);
    }
    assert_eq!(allocation.allocate(1_000_000)["a"], 333_300);
}

#[tokio::test]
async fn shared_percentages_lend_unused_capacity_to_history() {
    let blocks = vec![config(INSTRUCTIONS_BLOCK, 200), config("data", 100), config(HISTORY_BLOCK, 700)];
    let budgets = SharedContextBudget.allocate_sources(&blocks, ContextBudget { max_tokens: 1000, max_messages: 100 }, &[50, 25, 900]).await.unwrap();
    assert_eq!(budgets.iter().map(|budget| budget.max_tokens).collect::<Vec<_>>(), vec![50, 25, 925]);
    assert_eq!(budgets.iter().map(|budget| budget.max_tokens).sum::<usize>(), 1000);
}
