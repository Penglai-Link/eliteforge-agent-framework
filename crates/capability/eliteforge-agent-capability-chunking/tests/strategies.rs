use eliteforge_agent_capability_chunking::*;
use eliteforge_agent_core::*;
use std::sync::Arc;

fn block(id: &str, text: &str) -> ChunkBlock {
    ChunkBlock {
        id: id.into(),
        text: text.into(),
        kind: ChunkBlockKind::Paragraph,
        heading_path: vec!["Guide".into()],
        boundary: None,
        table: None,
        location: serde_json::json!({}),
    }
}
fn document(blocks: Vec<ChunkBlock>) -> ChunkDocument {
    ChunkDocument {
        id: "doc".into(),
        title: "Original guide".into(),
        blocks,
    }
}
fn options() -> ChunkingOptions {
    ChunkingOptions {
        max_chars: 120,
        overlap_chars: 12,
        max_context_chars: 480,
        ..Default::default()
    }
}

#[test]
fn automatic_chunks_keep_complete_cross_page_paragraphs_and_separate_index_context() {
    let mut first = block("a", &"完整条款。".repeat(8));
    first.location = serde_json::json!({"page":1});
    let mut second = block("b", "适用条件、例外和注意事项。");
    second.location = serde_json::json!({"page":2});
    let doc = document(vec![first, second]);
    let plan = built_in_chunk_strategies()
        .split("auto", &doc, &options())
        .unwrap();
    assert_eq!(plan.chunks.len(), 1);
    assert_eq!(plan.chunks[0].spans.len(), 2);
    assert!(!plan.chunks[0].text.contains("Original guide"));
    assert!(
        plan.chunks[0]
            .index_text
            .starts_with("Original guide\nGuide\n")
    );
    assert!(plan.contexts[0].text.contains("适用条件"));
}

#[test]
fn table_chunks_repeat_headers_and_footnotes_without_claiming_other_rows() {
    let header = "Item | value | unit\n";
    let footer = "Note: values refer to the original table.\n";
    let mut text = header.to_owned();
    let mut rows = Vec::new();
    for i in 0..12 {
        let start = text.chars().count();
        text.push_str(&format!("item-{i} | {i} | original-unit\n"));
        rows.push(ChunkTextRange {
            start,
            end: text.chars().count(),
        });
    }
    let start = text.chars().count();
    text.push_str(footer);
    let mut table = block("table", &text);
    table.kind = ChunkBlockKind::Table;
    table.table = Some(ChunkTable {
        header: ChunkTextRange {
            start: 0,
            end: header.chars().count(),
        },
        rows,
        footer: Some(ChunkTextRange {
            start,
            end: text.chars().count(),
        }),
    });
    let doc = document(vec![table]);
    let options = ChunkingOptions {
        max_chars: 200,
        max_context_chars: 800,
        ..options()
    };
    let plan = built_in_chunk_strategies()
        .split("auto", &doc, &options)
        .unwrap();
    assert!(plan.chunks.len() > 1);
    for chunk in &plan.chunks {
        assert!(chunk.text.starts_with(header));
        assert!(chunk.text.contains(footer.trim()));
        assert!(chunk.text.chars().count() <= options.max_chars);
    }
    for i in 0..12 {
        assert!(
            plan.chunks
                .iter()
                .any(|c| c.text.contains(&format!("item-{i} |")))
        );
    }
    assert!(
        plan.chunks[0]
            .spans
            .iter()
            .all(|s| s.start == 0 || s.start < 160 || s.start >= start)
    );
}

#[test]
fn fixed_windows_preserve_unicode_content_and_contexts_do_not_cross_worksheets() {
    let mut a = block("a", &"中文 original\n".repeat(60));
    a.boundary = Some("Sheet A".into());
    let mut b = block("b", "Sheet B original content.");
    b.boundary = Some("Sheet B".into());
    let doc = document(vec![a, b]);
    let registry = built_in_chunk_strategies();
    let plan = registry.split("fixed", &doc, &options()).unwrap();
    assert!(
        plan.contexts
            .iter()
            .all(|c| !(c.spans.iter().any(|s| s.block_id == "a")
                && c.spans.iter().any(|s| s.block_id == "b")))
    );
    let mut covered = vec![false; doc.blocks[0].text.chars().count()];
    for span in plan
        .chunks
        .iter()
        .flat_map(|c| &c.spans)
        .filter(|s| s.block_id == "a")
    {
        for index in span.start..span.end {
            covered[index] = true;
        }
    }
    assert!(covered.into_iter().all(|yes| yes));
    assert!(registry.split("unknown", &doc, &options()).is_err());
    assert!(
        registry
            .split(
                "fixed",
                &doc,
                &ChunkingOptions {
                    max_chunks: 1,
                    ..options()
                }
            )
            .is_err()
    );
}

struct Custom;
impl ChunkStrategy for Custom {
    fn descriptor(&self) -> ChunkStrategyDescriptor {
        ChunkStrategyDescriptor {
            id: "host.custom".into(),
            version: "2026-1".into(),
        }
    }
    fn split(
        &self,
        document: &ChunkDocument,
        options: &ChunkingOptions,
    ) -> Result<ChunkPlan, ChunkingError> {
        let mut plan = FixedChunkStrategy.split(document, options)?;
        plan.strategy = self.descriptor();
        Ok(plan)
    }
}
#[test]
fn hosts_register_custom_strategies_without_changing_a_framework_enum() {
    let mut registry = built_in_chunk_strategies();
    registry.register(Arc::new(Custom)).unwrap();
    assert!(registry.register(Arc::new(Custom)).is_err());
    let plan = registry
        .split(
            "host.custom",
            &document(vec![block("a", "Original evidence")]),
            &options(),
        )
        .unwrap();
    assert_eq!(plan.strategy.version, "2026-1");
}

#[test]
fn recall_uses_full_original_ranges_and_does_not_count_duplicate_hits() {
    let case = RetrievalEvaluationCase {
        id: "q".into(),
        query: "original question".into(),
        expected: vec![RetrievalEvidenceTarget {
            document_id: "doc".into(),
            span: ChunkSourceSpan {
                block_id: "b".into(),
                start: 10,
                end: 40,
            },
        }],
        partition: EvaluationPartition::Holdout,
        provenance: EvaluationProvenance::HumanLabeled,
    };
    let hit = |start, end| RetrievedEvidence {
        document_id: "doc".into(),
        spans: vec![ChunkSourceSpan {
            block_id: "b".into(),
            start,
            end,
        }],
    };
    let partial = score_retrieval_case(&case, &[hit(10, 25), hit(10, 25)], 8).unwrap();
    assert_eq!(partial.recall, 0.0);
    let full = score_retrieval_case(&case, &[hit(10, 25), hit(25, 40)], 8).unwrap();
    assert_eq!(full.recall, 1.0);
    assert_eq!(full.reciprocal_rank, 0.5);
    let foreign = RetrievedEvidence {
        document_id: "other".into(),
        spans: vec![case.expected[0].span.clone()],
    };
    assert_eq!(
        score_retrieval_case(&case, &[foreign], 8).unwrap().recall,
        0.0
    );
}

#[test]
fn oversized_table_notes_keep_all_source_content_in_bounded_continuations() {
    let header = "长表头。".repeat(40);
    let rows = "第一行\n第二行\n";
    let notes = "完整脚注和适用条件。".repeat(20);
    let text = format!("{header}{rows}{notes}");
    let start = header.chars().count();
    let end = start + rows.chars().count();
    let mut table = block("table", &text);
    table.kind = ChunkBlockKind::Table;
    table.table = Some(ChunkTable {
        header: ChunkTextRange {
            start: 0,
            end: start,
        },
        rows: vec![ChunkTextRange { start, end }],
        footer: Some(ChunkTextRange {
            start: end,
            end: text.chars().count(),
        }),
    });
    let plan = built_in_chunk_strategies()
        .split("auto", &document(vec![table]), &options())
        .unwrap();
    assert!(!plan.warnings.is_empty());
    assert!(
        plan.chunks
            .iter()
            .all(|c| c.text.chars().count() <= options().max_chars)
    );
    let case = RetrievalEvaluationCase {
        id: "table".into(),
        query: "完整脚注".into(),
        expected: vec![RetrievalEvidenceTarget {
            document_id: "doc".into(),
            span: ChunkSourceSpan {
                block_id: "table".into(),
                start: 0,
                end: text.chars().count(),
            },
        }],
        partition: EvaluationPartition::Holdout,
        provenance: EvaluationProvenance::HumanLabeled,
    };
    let hits = plan
        .chunks
        .iter()
        .map(|c| RetrievedEvidence {
            document_id: "doc".into(),
            spans: c.spans.clone(),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        score_retrieval_case(&case, &hits, hits.len())
            .unwrap()
            .recall,
        1.0
    );
}

#[test]
fn oversized_table_rows_continue_at_cells_with_complete_source_coverage() {
    let header = "项目 | 条件 | 单位\n";
    let row = format!(
        "{} | {} | {}\n",
        "完整适用条件和例外说明。".repeat(5),
        "剂量 0.05 mg/kg；".repeat(4),
        "保持所有原文与单位。".repeat(5)
    );
    let footer = "注：适用条件必须与剂量一起核对。";
    let start = header.chars().count();
    let end = start + row.chars().count();
    let original = format!("{header}{row}{footer}");
    let mut table = block("table", &original);
    table.kind = ChunkBlockKind::Table;
    table.table = Some(ChunkTable {
        header: ChunkTextRange {
            start: 0,
            end: start,
        },
        rows: vec![ChunkTextRange { start, end }],
        footer: Some(ChunkTextRange {
            start: end,
            end: original.chars().count(),
        }),
    });
    let plan = built_in_chunk_strategies()
        .split("auto", &document(vec![table]), &options())
        .unwrap();
    assert!(plan.chunks.len() > 1);
    assert!(
        plan.warnings.is_empty(),
        "a fully handled continuation is not an unresolved warning"
    );
    let mut spans = vec![];
    for chunk in &plan.chunks {
        assert!(chunk.text.starts_with(header));
        assert!(chunk.text.ends_with(footer));
        assert!(chunk.text.chars().count() <= options().max_chars);
        spans.extend(
            chunk
                .spans
                .iter()
                .filter(|s| s.start >= start && s.end <= end)
                .cloned(),
        );
    }
    spans.sort_by_key(|s| s.start);
    assert_eq!(spans.first().unwrap().start, start);
    assert_eq!(spans.last().unwrap().end, end);
    assert!(spans.windows(2).all(|p| p[0].end == p[1].start));
    let chars = original.chars().collect::<Vec<_>>();
    let restored = spans
        .iter()
        .flat_map(|s| chars[s.start..s.end].iter().copied())
        .collect::<String>();
    assert_eq!(
        restored, row,
        "no source characters, labels or units may be lost"
    );
}
