use super::*;
use std::collections::BTreeMap;

pub struct FixedChunkStrategy;
impl ChunkStrategy for FixedChunkStrategy {
    fn descriptor(&self) -> ChunkStrategyDescriptor {
        descriptor("fixed")
    }
    fn split(
        &self,
        document: &ChunkDocument,
        options: &ChunkingOptions,
    ) -> Result<ChunkPlan, ChunkingError> {
        options.validate()?;
        let mut chunks = Vec::new();
        let mut group = Vec::new();
        for block in &document.blocks {
            if group
                .last()
                .is_some_and(|previous: &&ChunkBlock| previous.boundary != block.boundary)
            {
                emit_window_group(&group, options, false, &mut chunks)?;
                group.clear();
            }
            group.push(block);
        }
        emit_window_group(&group, options, false, &mut chunks)?;
        finish_plan(document, options, self.descriptor(), chunks, Vec::new())
    }
}

pub(super) fn emit_window_group(
    group: &[&ChunkBlock],
    options: &ChunkingOptions,
    natural: bool,
    output: &mut Vec<DocumentChunk>,
) -> Result<(), ChunkingError> {
    let mut text = Vec::new();
    let mut ranges = Vec::new();
    for block in group {
        if !text.is_empty() {
            text.push('\n');
        }
        let start = text.len();
        text.extend(block.text.chars());
        ranges.push((start, text.len(), *block));
    }
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + options.max_chars).min(text.len());
        if natural && end < text.len() {
            if let Some(boundary) = (start + options.max_chars / 2..end).rev().find(|&i| {
                matches!(text[i], '\n' | '。' | '！' | '？')
                    || (matches!(text[i], '.' | '!' | '?')
                        && text.get(i + 1).is_none_or(|c| c.is_whitespace()))
            }) {
                end = boundary + 1;
            }
        }
        let body: String = text[start..end].iter().collect();
        if !body.trim().is_empty() {
            let spans: Vec<_> = ranges
                .iter()
                .filter_map(|(begin, finish, block)| {
                    let a = start.max(*begin);
                    let b = end.min(*finish);
                    (a < b).then(|| ChunkSourceSpan {
                        block_id: block.id.clone(),
                        start: a - begin,
                        end: b - begin,
                    })
                })
                .collect();
            let headings = ranges
                .iter()
                .find(|(begin, finish, _)| *begin < end && *finish > start)
                .map(|(_, _, b)| b.heading_path.clone())
                .unwrap_or_default();
            push_chunk(output, body, headings, spans, options)?;
        }
        if end == text.len() {
            break;
        }
        start = end.saturating_sub(options.overlap_chars).max(start + 1);
    }
    Ok(())
}

pub(super) fn push_chunk(
    output: &mut Vec<DocumentChunk>,
    text: String,
    heading_path: Vec<String>,
    spans: Vec<ChunkSourceSpan>,
    options: &ChunkingOptions,
) -> Result<(), ChunkingError> {
    if output.len() >= options.max_chunks {
        return Err(ChunkingError(
            "document exceeds the chunk count budget".into(),
        ));
    }
    output.push(DocumentChunk {
        id: format!("chunk-{}", output.len()),
        index_text: String::new(),
        text,
        heading_path,
        spans,
        context_id: String::new(),
    });
    Ok(())
}

pub(super) fn render_spans(document: &ChunkDocument, spans: &[ChunkSourceSpan]) -> String {
    let mut ranges: BTreeMap<&str, Vec<(usize, usize)>> = BTreeMap::new();
    for span in spans {
        ranges
            .entry(&span.block_id)
            .or_default()
            .push((span.start, span.end));
    }
    let mut text = Vec::new();
    for block in &document.blocks {
        let Some(ranges) = ranges.get_mut(block.id.as_str()) else {
            continue;
        };
        ranges.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for &(start, end) in ranges.iter() {
            if let Some(previous) = merged.last_mut().filter(|p| start <= p.1) {
                previous.1 = previous.1.max(end);
            } else {
                merged.push((start, end));
            }
        }
        let chars: Vec<_> = block.text.chars().collect();
        for (start, end) in merged {
            text.push(chars[start..end].iter().collect::<String>());
        }
    }
    text.join("\n")
}

pub(super) fn finish_plan(
    document: &ChunkDocument,
    options: &ChunkingOptions,
    strategy: ChunkStrategyDescriptor,
    mut chunks: Vec<DocumentChunk>,
    warnings: Vec<String>,
) -> Result<ChunkPlan, ChunkingError> {
    let mut contexts = Vec::new();
    let mut start = 0;
    while start < chunks.len() {
        let mut end = start;
        let mut size = 0;
        let boundary = |chunk: &DocumentChunk| {
            document
                .blocks
                .iter()
                .find(|block| {
                    chunk
                        .spans
                        .first()
                        .is_some_and(|span| span.block_id == block.id)
                })
                .and_then(|block| block.boundary.as_ref())
        };
        while end < chunks.len()
            && chunks[end].heading_path == chunks[start].heading_path
            && boundary(&chunks[end]) == boundary(&chunks[start])
        {
            let next = chunks[end].text.chars().count() + 1;
            if end > start && size + next > options.max_context_chars {
                break;
            }
            size += next;
            end += 1;
        }
        let spans: Vec<_> = chunks[start..end]
            .iter()
            .flat_map(|c| c.spans.clone())
            .collect();
        let context_id = format!("context-{}", contexts.len());
        let text = render_spans(document, &spans);
        if text.chars().count() > options.max_context_chars {
            return Err(ChunkingError("source context exceeds its budget".into()));
        }
        contexts.push(ChunkContext {
            id: context_id.clone(),
            text,
            spans,
        });
        for chunk in &mut chunks[start..end] {
            chunk.context_id = context_id.clone();
            let context = [document.title.as_str(), &chunk.heading_path.join(" > ")]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            chunk.index_text = if context.is_empty() {
                chunk.text.clone()
            } else {
                format!("{context}\n{}", chunk.text)
            };
        }
        start = end;
    }
    Ok(ChunkPlan {
        strategy,
        chunks,
        contexts,
        warnings,
    })
}
