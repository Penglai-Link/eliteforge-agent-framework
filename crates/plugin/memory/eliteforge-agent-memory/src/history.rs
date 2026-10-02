//! Select whole turns without separating tool calls from their matching results.
#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub sequence: i64,
    pub role: String,
    pub run_id: Option<String>,
    pub tool_calls: Vec<String>,
    pub tool_results: Vec<String>,
    pub estimated_tokens: u32,
}
#[derive(Debug, Clone, Copy)]
pub struct HistorySelection {
    pub tail_start: usize,
    pub pinned_start: usize,
    pub tail_tokens: u32,
}
fn boundary(entries: &[HistoryEntry], index: usize) -> usize {
    if entries.is_empty() {
        return 0;
    }
    let bounded = index.min(entries.len() - 1);
    let mut start = entries[..=bounded]
        .iter()
        .rposition(|e| e.role == "user")
        .unwrap_or(0);
    // Malformed legacy boundaries are expanded, never shortened. Match IDs, not
    // simply the nearest assistant, including parallel tool batches.
    loop {
        let before = start;
        for entry in &entries[start..] {
            for result in &entry.tool_results {
                if let Some(call) = entries[..start]
                    .iter()
                    .rposition(|e| e.tool_calls.contains(result))
                {
                    start = start.min(call);
                }
            }
        }
        if start == before {
            break;
        }
        start = entries[..=start]
            .iter()
            .rposition(|e| e.role == "user")
            .unwrap_or(0);
    }
    start
}
pub fn select_history(
    entries: &[HistoryEntry],
    retained_tokens: u32,
    pinned_run: Option<&str>,
    requested_sequence: Option<i64>,
) -> HistorySelection {
    if entries.is_empty() {
        return HistorySelection {
            tail_start: 0,
            pinned_start: 0,
            tail_tokens: 0,
        };
    }
    let latest = boundary(entries, entries.len() - 1);
    let pinned = pinned_run
        .and_then(|run| {
            entries
                .iter()
                .position(|e| e.run_id.as_deref() == Some(run))
        })
        .map(|i| boundary(entries, i))
        .unwrap_or(latest)
        .min(latest);
    let mut start = requested_sequence
        .map(|seq| {
            boundary(
                entries,
                entries
                    .iter()
                    .position(|e| e.sequence >= seq)
                    .unwrap_or(entries.len() - 1),
            )
        })
        .unwrap_or(pinned)
        .min(pinned);
    let count = |from: usize| {
        entries[from..]
            .iter()
            .fold(0u32, |sum, e| sum.saturating_add(e.estimated_tokens))
    };
    if requested_sequence.is_none() {
        while start > 0 {
            let previous = boundary(entries, start - 1);
            if previous >= start || count(previous) > retained_tokens {
                break;
            }
            start = previous;
        }
    }
    HistorySelection {
        tail_start: start,
        pinned_start: pinned,
        tail_tokens: count(start),
    }
}
