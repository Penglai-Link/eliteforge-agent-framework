use eliteforge_agent_core::MemoryError;
use eliteforge_agent_memory::*;
fn entry(seq: i64, role: &str, run: &str, calls: &[&str], results: &[&str]) -> HistoryEntry {
    HistoryEntry {
        sequence: seq,
        role: role.into(),
        run_id: Some(run.into()),
        tool_calls: calls.iter().map(|s| s.to_string()).collect(),
        tool_results: results.iter().map(|s| s.to_string()).collect(),
        estimated_tokens: 10,
    }
}
#[test]
fn latest_complete_turn_and_parallel_tool_batch_survive_zero_tail_budget() {
    let entries = vec![
        entry(1, "user", "old", &[], &[]),
        entry(2, "assistant", "old", &[], &[]),
        entry(3, "user", "new", &[], &[]),
        entry(4, "assistant", "new", &["a", "b"], &[]),
        entry(5, "tool", "new", &[], &["b"]),
        entry(6, "tool", "new", &[], &["a"]),
        entry(7, "assistant", "new", &[], &[]),
    ];
    for requested in [None, Some(5), Some(100)] {
        let selected = select_history(&entries, 0, Some("new"), requested);
        assert_eq!(selected.tail_start, 2);
        assert_eq!(selected.tail_tokens, 50);
    }
    assert_eq!(entries[2].sequence, 3);
}
#[test]
fn replay_expands_to_matching_call_id_across_an_invalid_legacy_turn_boundary() {
    let entries = vec![
        entry(1, "user", "a", &[], &[]),
        entry(2, "assistant", "a", &["exact"], &[]),
        entry(3, "user", "b", &[], &[]),
        entry(4, "assistant", "b", &["nearby"], &[]),
        entry(5, "tool", "b", &[], &["exact"]),
    ];
    assert_eq!(select_history(&entries, 0, None, None).tail_start, 0);
}
#[test]
fn retention_admits_only_whole_earlier_turns_and_clamps_arithmetic() {
    let entries = vec![
        entry(1, "user", "a", &[], &[]),
        entry(2, "assistant", "a", &[], &[]),
        entry(3, "user", "b", &[], &[]),
        entry(4, "assistant", "b", &[], &[]),
    ];
    assert_eq!(select_history(&entries, 39, None, None).tail_start, 2);
    assert_eq!(select_history(&entries, 40, None, None).tail_start, 0);
    assert_eq!(ratio_tokens(u32::MAX, 10_000), u32::MAX);
    assert_eq!(ratio_tokens(100, 90_000), 100);
}
#[test]
fn block_order_actual_metadata_budget_and_output_reserve_are_independent() {
    let scope = MemoryScope::new("user-a/workspace", "session-a").unwrap();
    let source = MemorySnapshot {
        revision: MemoryRevision {
            scope: scope.clone(),
            generation: "one".into(),
            revision: 1,
        },
        blocks: vec![
            MemoryBlock {
                kind: MemoryBlockKind::CurrentSessionHistory,
                entries: vec!["full latest turn".to_string()],
            },
            MemoryBlock {
                kind: MemoryBlockKind::ConversationHistorySummary,
                entries: vec!["é".repeat(500)],
            },
            MemoryBlock {
                kind: MemoryBlockKind::SeedPromptPrefix,
                entries: vec!["trusted instructions".into()],
            },
        ],
    };
    let mut budgets = ContextBlockBudgets::default();
    budgets.conversation_history_summary.max_tokens = Some(80);
    let plan = budgets.plan(1_000, 200);
    let assembled = ShortTermMemoryEngine::assemble(
        &source,
        &scope,
        &plan,
        |text| estimate_serialized_tokens(&serde_json::json!({"metadata":"source","text":text})),
        |text, max| bound_text(&text, (max * 4) as usize),
    )
    .unwrap();
    assert_eq!(assembled.messages.first().unwrap(), "trusted instructions");
    assert_eq!(assembled.messages.last().unwrap(), "full latest turn");
    let checkpoint = assembled
        .reports
        .iter()
        .find(|r| r.kind == MemoryBlockKind::ConversationHistorySummary)
        .unwrap();
    assert!(checkpoint.tokens <= 80);
    assert_eq!(checkpoint.entries, 1);
    assert_eq!(source.blocks[1].entries[0], "é".repeat(500));
    assert_eq!(plan.effective_input_budget, 800);
    let assessed = assess_request(&plan, 750, 20, 120, 100, 8_000, 2_000, true);
    assert!(assessed.over_budget);
    assert_eq!(assessed.estimated_tokens, 850);
    assert_eq!(assessed.history_budget_tokens, 580);
    assert_eq!(assessed.retained_history_tokens, 116);
}
struct Store {
    current: MemoryRevision,
    projection: String,
}
impl ShortTermMemoryProjectionStore<String> for Store {
    fn compare_exchange_projection(
        &mut self,
        expected: &MemoryRevision,
        value: String,
    ) -> Result<(), MemoryError> {
        expected.ensure_current(&self.current)?;
        self.projection = value;
        self.current.revision += 1;
        Ok(())
    }
}
#[test]
fn scoped_projection_write_restore_and_stale_results_never_cross_users_or_sessions() {
    let original = MemoryRevision {
        scope: MemoryScope::new("user-a", "session-a").unwrap(),
        generation: "generation-a".into(),
        revision: 2,
    };
    let mut store = Store {
        current: original.clone(),
        projection: "old".into(),
    };
    ShortTermMemoryEngine::write_projection(&mut store, &original, "new".into()).unwrap();
    assert_eq!(store.projection, "new");
    assert!(ShortTermMemoryEngine::write_projection(&mut store, &original, "late".into()).is_err());
    assert_eq!(store.projection, "new");
    for (user, session) in [("user-b", "session-a"), ("user-a", "session-b")] {
        let scope = MemoryScope::new(user, session).unwrap();
        let wrong = MemoryRevision {
            scope: scope.clone(),
            ..store.current.clone()
        };
        assert!(
            ShortTermMemoryEngine::write_projection(&mut store, &wrong, "crossed".into()).is_err()
        );
        let source = MemorySnapshot::<String> {
            revision: store.current.clone(),
            blocks: vec![],
        };
        assert!(
            ShortTermMemoryEngine::assemble(
                &source,
                &scope,
                &ContextBlockBudgets::default().plan(100, 0),
                |_| 1,
                |value, _| value
            )
            .is_err()
        );
    }
    assert_eq!(bound_text("文".repeat(50).as_str(), 1).chars().count(), 1);
    assert!(bound_text("long", 0).is_empty());
}

#[test]
fn block_bound_keeps_the_largest_feasible_target_including_json_framing() {
    for text in ["x".repeat(200), "文".repeat(200)] {
        let scope = MemoryScope::new("workspace", "session").unwrap();
        let mut budgets = ContextBlockBudgets::default();
        budgets.conversation_history_summary.max_tokens = Some(80);
        budgets.conversation_history_summary.max_ratio_bps = None;
        let source = MemorySnapshot {
            revision: MemoryRevision {
                scope: scope.clone(),
                generation: "source".into(),
                revision: 1,
            },
            blocks: vec![MemoryBlock {
                kind: MemoryBlockKind::ConversationHistorySummary,
                entries: vec![text.clone()],
            }],
        };
        let assembled = ShortTermMemoryEngine::assemble(
            &source,
            &scope,
            &budgets.plan(1_000, 100),
            |text| {
                estimate_serialized_tokens(&serde_json::json!({"metadata":"source","text":text}))
            },
            |text, target| bound_text(&text, target as usize),
        )
        .unwrap();
        let report = assembled
            .reports
            .iter()
            .find(|r| r.kind == MemoryBlockKind::ConversationHistorySummary)
            .unwrap();
        let estimate = |text: &str| {
            estimate_serialized_tokens(&serde_json::json!({"metadata":"source","text":text}))
        };
        // Exhaustively verify the bound contract rather than assuming the text's
        // character count equals its serialized Unicode-aware token estimate.
        let (target, expected) = (0..=80)
            .map(|target| (target, bound_text(&text, target)))
            .rfind(|(_, candidate)| estimate(candidate) <= 80)
            .unwrap();
        assert_eq!(assembled.messages, vec![expected.clone()]);
        assert_eq!(report.tokens, estimate(&expected));
        assert!(report.tokens <= 80);
        if target < 80 {
            assert!(estimate(&bound_text(&text, target + 1)) > 80);
        }
        assert_eq!(report.entries, 1);
        assert_eq!(source.blocks[0].entries[0], text);
    }
}

#[test]
fn disabled_mandatory_blocks_keep_complete_inputs_and_report_budget_overflow() {
    let scope = MemoryScope::new("workspace", "session").unwrap();
    let source = MemorySnapshot {
        revision: MemoryRevision {
            scope: scope.clone(),
            generation: "source".into(),
            revision: 1,
        },
        blocks: vec![
            MemoryBlock {
                kind: MemoryBlockKind::SeedPromptPrefix,
                entries: vec!["trusted instructions".to_owned()],
            },
            MemoryBlock {
                kind: MemoryBlockKind::CurrentSessionHistory,
                entries: vec!["complete current user input with attachments".to_owned()],
            },
        ],
    };
    let mut plan = ContextBlockBudgets::default().plan(1000, 100);
    plan.block_budgets.seed_prompt_prefix.enabled = false;
    plan.block_budgets.current_session_history.enabled = false;
    plan.block_budgets.seed_prompt_prefix.effective_limit_tokens = 0;
    let assembled = ShortTermMemoryEngine::assemble(
        &source,
        &scope,
        &plan,
        estimate_serialized_tokens,
        |_, _| panic!("mandatory inputs must not be bounded"),
    )
    .unwrap();
    assert_eq!(
        assembled.messages,
        vec![
            "trusted instructions",
            "complete current user input with attachments"
        ]
    );
    let prefix = &assembled.reports[0];
    assert!(prefix.tokens > prefix.budget_tokens);
    let assessed = assess_request(
        &plan,
        assembled.reports.iter().map(|r| r.tokens).sum(),
        prefix.tokens,
        prefix.tokens,
        0,
        8000,
        2000,
        false,
    );
    assert!(assessed.over_budget);
    assert_eq!(assessed.reason, Some("SEED_PROMPT_PREFIX_OVER_BUDGET"));
}

#[test]
fn platform_memory_scope_does_not_alias_a_workspace_namespace_or_cache_key() {
    let platform = MemoryScope::platform("same-session").unwrap();
    let named = MemoryScope::new("platform", "same-session").unwrap();
    assert!(platform.is_platform());
    assert_ne!(platform, named);
    assert_ne!(platform.storage_key(), named.storage_key());
    assert_ne!(
        MemoryScope::new("a:b", "c").unwrap().storage_key(),
        MemoryScope::new("a", "b:c").unwrap().storage_key()
    );
    assert!(MemoryScope::platform("").is_err());
}
