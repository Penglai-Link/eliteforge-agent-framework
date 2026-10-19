use super::*;
use splitting::{emit_window_group, finish_plan, push_chunk};

pub struct AutomaticChunkStrategy;
pub struct StructuralChunkStrategy;
pub struct HeadingChunkStrategy;

impl ChunkStrategy for AutomaticChunkStrategy {
    fn descriptor(&self) -> ChunkStrategyDescriptor {
        descriptor("auto")
    }
    fn split(
        &self,
        document: &ChunkDocument,
        options: &ChunkingOptions,
    ) -> Result<ChunkPlan, ChunkingError> {
        // Local structure is sufficient for the default. No additional model
        // calls are needed to preserve paragraphs, lists, tables or citations.
        if !document.blocks.is_empty()
            && document
                .blocks
                .iter()
                .all(|block| block.kind == ChunkBlockKind::Code)
        {
            let mut plan = FixedChunkStrategy.split(document, options)?;
            plan.strategy = self.descriptor();
            return Ok(plan);
        }
        structural(document, options, self.descriptor(), true)
    }
}
impl ChunkStrategy for StructuralChunkStrategy {
    fn descriptor(&self) -> ChunkStrategyDescriptor {
        descriptor("structure")
    }
    fn split(
        &self,
        document: &ChunkDocument,
        options: &ChunkingOptions,
    ) -> Result<ChunkPlan, ChunkingError> {
        structural(document, options, self.descriptor(), true)
    }
}
impl ChunkStrategy for HeadingChunkStrategy {
    fn descriptor(&self) -> ChunkStrategyDescriptor {
        descriptor("heading")
    }
    fn split(
        &self,
        document: &ChunkDocument,
        options: &ChunkingOptions,
    ) -> Result<ChunkPlan, ChunkingError> {
        structural(document, options, self.descriptor(), false)
    }
}

fn structural(
    document: &ChunkDocument,
    options: &ChunkingOptions,
    strategy: ChunkStrategyDescriptor,
    preserve_blocks: bool,
) -> Result<ChunkPlan, ChunkingError> {
    options.validate()?;
    let mut chunks = Vec::new();
    let mut group: Vec<&ChunkBlock> = Vec::new();
    let mut size = 0;
    let mut warnings = Vec::new();
    for block in &document.blocks {
        let count = block.text.chars().count();
        let boundary = group.last().is_some_and(|previous| {
            previous.heading_path != block.heading_path || previous.boundary != block.boundary
        });
        let table = preserve_blocks && block.kind == ChunkBlockKind::Table;
        if boundary || table || (preserve_blocks && size + count + 1 > options.max_chars) {
            emit_window_group(&group, options, true, &mut chunks)?;
            group.clear();
            size = 0;
        }
        if table {
            if let Some(table) = &block.table {
                emit_table(block, table, options, &mut chunks, &mut warnings)?;
            } else {
                emit_window_group(&[block], options, true, &mut chunks)?;
                if count > options.max_chars {
                    warnings.push(format!(
                        "table {} has no row structure; verify its continuation chunks",
                        block.id
                    ));
                }
            }
        } else {
            group.push(block);
            size += count + 1;
        }
    }
    emit_window_group(&group, options, true, &mut chunks)?;
    finish_plan(document, options, strategy, chunks, warnings)
}

fn emit_table(
    block: &ChunkBlock,
    table: &ChunkTable,
    options: &ChunkingOptions,
    output: &mut Vec<DocumentChunk>,
    warnings: &mut Vec<String>,
) -> Result<(), ChunkingError> {
    let chars: Vec<_> = block.text.chars().collect();
    let ranges = std::iter::once(table.header)
        .chain(table.rows.iter().copied())
        .chain(table.footer);
    if ranges
        .clone()
        .any(|r| r.start > r.end || r.end > chars.len())
    {
        return Err(ChunkingError("table source ranges are invalid".into()));
    }
    let header: String = chars[table.header.start..table.header.end].iter().collect();
    let footer = table
        .footer
        .map(|r| chars[r.start..r.end].iter().collect::<String>())
        .unwrap_or_default();
    let extra = header.chars().count() + footer.chars().count() + 2;
    if extra + 32 >= options.max_chars {
        emit_window_group(&[block], options, true, output)?;
        warnings.push(format!("table {} has oversized headers or footnotes; preserved as source-linked continuation chunks",block.id));
        return Ok(());
    }
    let available = options.max_chars - extra;
    let mut rows = Vec::new();
    for row in &table.rows {
        let mut start = row.start;
        while start < row.end {
            let mut end = (start + available).min(row.end);
            if end < row.end {
                // Prefer a cell boundary, then a sentence boundary. Continuation
                // ranges remain adjacent and lossless, including all separators.
                let candidates = start + available / 2..end;
                let boundary = candidates
                    .clone()
                    .rev()
                    .find(|&i| matches!(chars[i], '|' | '\t'))
                    .or_else(|| {
                        candidates
                            .rev()
                            .find(|&i| matches!(chars[i], '\n' | '。' | '；' | ';'))
                    });
                if let Some(boundary) = boundary {
                    end = boundary + 1;
                }
            }
            rows.push(ChunkTextRange { start, end });
            start = end;
        }
    }
    if rows.is_empty() {
        emit_window_group(&[block], options, true, output)?;
        return Ok(());
    }
    let mut start = 0;
    while start < rows.len() {
        let mut end = start;
        let mut size = 0;
        while end < rows.len() && size + rows[end].end - rows[end].start + 1 <= available {
            size += rows[end].end - rows[end].start + 1;
            end += 1;
        }
        // A maximal row already occupies available characters; permit it alone.
        if end == start {
            end += 1;
        }
        let spans: Vec<_> = std::iter::once(table.header)
            .chain(rows[start..end].iter().copied())
            .chain(table.footer)
            .filter(|r| r.start < r.end)
            .map(|r| ChunkSourceSpan {
                block_id: block.id.clone(),
                start: r.start,
                end: r.end,
            })
            .collect();
        let mut text = header.clone();
        for row in &rows[start..end] {
            text.push('\n');
            text.extend(chars[row.start..row.end].iter());
        }
        if !footer.is_empty() {
            text.push('\n');
            text.push_str(&footer);
        }
        push_chunk(output, text, block.heading_path.clone(), spans, options)?;
        start = end;
    }
    Ok(())
}
