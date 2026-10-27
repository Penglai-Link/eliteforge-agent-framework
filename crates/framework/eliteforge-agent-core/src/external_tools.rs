//! Native external observations share one tool identity across lifecycle updates.
use crate::{HistoryStatus, HistoryToolInvocation};
use serde_json::Value;

/// Only explicit native tool metadata can establish a result. Parent run success
/// never implies that an opaque tool completed.
pub fn external_tool_status(detail: &Value) -> HistoryStatus {
    let status = detail.get("status").and_then(Value::as_str).unwrap_or("");
    match status {
        "completed" | "success" | "succeeded"
            if detail
                .get("exitCode")
                .and_then(Value::as_i64)
                .is_some_and(|code| code != 0) =>
        {
            HistoryStatus::Failed
        }
        "completed" | "success" | "succeeded" => HistoryStatus::Completed,
        "failed" | "error" => HistoryStatus::Failed,
        "cancelled" | "canceled" | "interrupted" => HistoryStatus::Interrupted,
        "in_progress" | "running" | "pending" => HistoryStatus::Running,
        _ => HistoryStatus::Unknown,
    }
}

fn native_id(detail: &Value) -> Option<&str> {
    detail
        .get("callId")
        .or_else(|| detail.get("id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
}

/// Resolve persisted native lifecycle observations on read as well as new events.
/// Ordinary framework tool records and observations without a native ID stay intact.
pub fn merge_external_tool_observation(
    tools: &mut Vec<HistoryToolInvocation>,
    mut next: HistoryToolInvocation,
) -> HistoryToolInvocation {
    let detail = next.structured.as_ref();
    let native = detail.filter(|detail| {
        matches!(
            detail.get("event").and_then(Value::as_str),
            Some("item.started" | "item.updated" | "item.completed" | "tool_use")
        )
    });
    let id = native.and_then(native_id).map(str::to_owned);
    if let Some(detail) = native {
        let status = external_tool_status(detail);
        if status != HistoryStatus::Unknown {
            next.status = status;
        }
        next.is_error = match next.status {
            HistoryStatus::Completed => Some(false),
            HistoryStatus::Failed => Some(true),
            _ => None,
        };
    }
    if let Some(id) = &id {
        next.call_id = id.clone();
        if let Some(current) = tools.iter_mut().find(|tool| {
            tool.run_id == next.run_id
                && tool.tool_name == next.tool_name
                && tool.structured.as_ref().and_then(native_id) == Some(id.as_str())
        }) {
            let terminal = |status| {
                matches!(
                    status,
                    HistoryStatus::Completed | HistoryStatus::Failed | HistoryStatus::Interrupted
                )
            };
            if !terminal(current.status) || terminal(next.status) {
                current.status = next.status;
                current.is_error = next.is_error;
                current.structured = next.structured;
                if !next.content.is_empty() {
                    current.content = next.content;
                }
                if !next.arguments.is_null() {
                    current.arguments = next.arguments;
                }
                current.finished_at_ms = next.finished_at_ms.or(current.finished_at_ms);
            }
            current.call_id = id.clone();
            current.started_at_ms = current.started_at_ms.or(next.started_at_ms);
            return current.clone();
        }
    }
    tools.push(next.clone());
    next
}

/// A native start without its result becomes uncertain when its owning run ends.
/// The parent result cannot establish whether that command succeeded.
pub fn finalize_external_tool_observation(tool: &mut HistoryToolInvocation) {
    if tool.status == HistoryStatus::Running
        && tool.structured.as_ref().is_some_and(|detail| {
            matches!(
                detail.get("event").and_then(Value::as_str),
                Some("item.started" | "item.updated" | "item.completed" | "tool_use")
            )
        })
    {
        tool.status = HistoryStatus::Unknown;
        tool.is_error = None;
    }
}

pub fn normalize_external_tool_history(tools: &mut Vec<HistoryToolInvocation>) {
    let original = std::mem::take(tools);
    for tool in original {
        merge_external_tool_observation(tools, tool);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RunId;
    use serde_json::json;
    fn tool(run: RunId, id: &str, status: &str) -> HistoryToolInvocation {
        HistoryToolInvocation {
            batch_revision: None,
            batch: None,
            id: RunId::new_v4(),
            run_id: run,
            step_id: "message".into(),
            message_id: "message".into(),
            call_id: String::new(),
            tool_id: "command_execution".into(),
            tool_name: "command_execution".into(),
            arguments: Value::Null,
            status: HistoryStatus::Unknown,
            content: vec![],
            structured: Some(json!({"event":"item.updated","id":id,"status":status})),
            is_error: None,
            result_reference: None,
            started_at_ms: Some(1),
            finished_at_ms: None,
        }
    }
    #[test]
    fn native_lifecycle_has_one_stable_identity_and_does_not_regress() {
        let run = RunId::new_v4();
        let first = tool(run, "item-1", "in_progress");
        let id = first.id;
        let mut tools = vec![
            first,
            tool(run, "item-1", "completed"),
            tool(run, "item-1", "in_progress"),
        ];
        normalize_external_tool_history(&mut tools);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].id, id);
        assert_eq!(tools[0].call_id, "item-1");
        assert_eq!(tools[0].status, HistoryStatus::Completed);
        assert_eq!(tools[0].is_error, Some(false));
        merge_external_tool_observation(&mut tools, tool(run, "item-2", "failed"));
        merge_external_tool_observation(&mut tools, tool(RunId::new_v4(), "item-1", "completed"));
        assert_eq!(tools.len(), 3);
        assert_eq!(tools[1].status, HistoryStatus::Failed);
    }
    #[test]
    fn opaque_or_incomplete_native_results_stay_unknown() {
        assert_eq!(external_tool_status(&json!({})), HistoryStatus::Unknown);
        let run = RunId::new_v4();
        let mut tools = vec![tool(run, "item-1", "")];
        normalize_external_tool_history(&mut tools);
        assert_eq!(tools[0].status, HistoryStatus::Unknown);
        let mut pending = tool(run, "pending", "in_progress");
        pending.status = HistoryStatus::Running;
        finalize_external_tool_observation(&mut pending);
        assert_eq!(pending.status, HistoryStatus::Unknown);
        assert_eq!(
            external_tool_status(&json!({"status":"completed","exitCode":2})),
            HistoryStatus::Failed
        );
        assert_eq!(
            external_tool_status(&json!({"status":"cancelled"})),
            HistoryStatus::Interrupted
        );
    }
}
