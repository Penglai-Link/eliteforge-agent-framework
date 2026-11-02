use serde_json::Value;

use crate::{ExternalAgentEvent as ModelStreamEvent, ExternalAgentUsage as ModelTokenUsage};

use crate::ExternalCliKind as SystemAgentCliKind;

#[derive(Debug, Default)]
pub(super) struct ParsedSystemAgentOutput {
    pub(super) final_output: Option<String>,
    pub(super) token_usage: Option<ModelTokenUsage>,
    pub(super) system_agent_session_id: Option<String>,
}

pub(super) fn parse_complete_stdout(
    kind: SystemAgentCliKind,
    stdout: &str,
) -> ParsedSystemAgentOutput {
    let mut parsed = ParsedSystemAgentOutput::default();
    for line in stdout.lines() {
        parse_stream_line(kind, line, &mut parsed);
    }
    parsed
}

pub(super) fn parse_stream_line(
    kind: SystemAgentCliKind,
    line: &str,
    parsed: &mut ParsedSystemAgentOutput,
) -> Vec<ModelStreamEvent> {
    let previous_session = parsed.system_agent_session_id.clone();
    let mut events = match kind {
        SystemAgentCliKind::Dsh => Vec::new(),
        SystemAgentCliKind::Codex => parse_codex_stdout_line(line, parsed),
        SystemAgentCliKind::OpenCode => parse_opencode_stdout_line(line, parsed),
    };
    if previous_session.is_none()
        && let Some(session_id) = parsed.system_agent_session_id.as_ref()
    {
        events.insert(0, ModelStreamEvent::session_observed(session_id.clone()));
    }
    events
}

fn parse_codex_stdout_line(
    line: &str,
    parsed: &mut ParsedSystemAgentOutput,
) -> Vec<ModelStreamEvent> {
    let Some(value) = parse_json_line(line) else {
        return Vec::new();
    };
    if parsed.system_agent_session_id.is_none() {
        parsed.system_agent_session_id = codex_session_id(&value);
    }
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if event_type.starts_with("approval.")
        || value
            .get("item")
            .and_then(|item| item.get("type"))
            .and_then(Value::as_str)
            == Some("approval_request")
    {
        return vec![ModelStreamEvent::approval_observed(observation_detail(
            &value,
        ))];
    }
    if matches!(
        event_type,
        "item.started" | "item.updated" | "item.completed"
    ) && let Some(item) = value.get("item")
        && let Some(item_type) = item.get("type").and_then(Value::as_str)
        && matches!(
            item_type,
            "command_execution"
                | "mcp_tool_call"
                | "web_search"
                | "file_change"
                | "function_call"
                | "tool_call"
        )
    {
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(item_type);
        return vec![ModelStreamEvent::tool_observed(
            name,
            observation_detail(&value),
        )];
    }
    if value.get("type").and_then(Value::as_str) == Some("item.completed")
        && value
            .get("item")
            .and_then(|item| item.get("type"))
            .and_then(Value::as_str)
            == Some("agent_message")
        && let Some(text) = value
            .get("item")
            .and_then(|item| item.get("text"))
            .and_then(Value::as_str)
    {
        parsed.final_output = Some(text.to_string());
    }
    if value.get("type").and_then(Value::as_str) == Some("turn.completed")
        && let Some(usage) = value.get("usage")
    {
        parsed.token_usage = Some(ModelTokenUsage {
            input_tokens: usage
                .get("input_tokens")
                .and_then(Value::as_u64)
                .and_then(to_u32)
                .unwrap_or(0),
            output_tokens: usage
                .get("output_tokens")
                .and_then(Value::as_u64)
                .and_then(to_u32)
                .unwrap_or(0),
            total_tokens: usage
                .get("input_tokens")
                .and_then(Value::as_u64)
                .and_then(to_u32)
                .unwrap_or(0)
                .saturating_add(
                    usage
                        .get("output_tokens")
                        .and_then(Value::as_u64)
                        .and_then(to_u32)
                        .unwrap_or(0),
                ),
            cached_input_tokens: None,
            reasoning_tokens: None,
        });
    }
    if value
        .get("item")
        .and_then(|item| item.get("type"))
        .and_then(Value::as_str)
        == Some("reasoning")
    {
        let reasoning_delta = first_non_empty_string(
            &value,
            &[
                &["item", "delta"],
                &["item", "text_delta"],
                &["item", "text"],
                &["item", "summary"],
            ],
        );
        if let Some(delta) = reasoning_delta {
            return vec![ModelStreamEvent::reasoning_summary_delta(delta)];
        }
    }
    if value.get("type").and_then(Value::as_str) == Some("item.updated")
        && value
            .get("item")
            .and_then(|item| item.get("type"))
            .and_then(Value::as_str)
            == Some("agent_message")
        && let Some(delta) = first_non_empty_string(
            &value,
            &[
                &["item", "delta"],
                &["item", "text_delta"],
                &["item", "text"],
            ],
        )
    {
        return vec![ModelStreamEvent::text_delta(delta)];
    }
    if matches!(
        event_type,
        "turn.started" | "turn.completed" | "turn.failed" | "error"
    ) {
        return vec![ModelStreamEvent::status(event_type)];
    }
    Vec::new()
}

fn parse_opencode_stdout_line(
    line: &str,
    parsed: &mut ParsedSystemAgentOutput,
) -> Vec<ModelStreamEvent> {
    let Some(value) = parse_json_line(line) else {
        return Vec::new();
    };
    if parsed.system_agent_session_id.is_none() {
        parsed.system_agent_session_id = opencode_session_id(&value);
    }
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if event_type.starts_with("permission.") || event_type.starts_with("approval.") {
        return vec![ModelStreamEvent::approval_observed(observation_detail(
            &value,
        ))];
    }
    let mut events = Vec::new();
    if matches!(event_type, "step_start" | "step_finish" | "error") {
        events.push(ModelStreamEvent::status(event_type));
    }
    if value.get("type").and_then(Value::as_str) == Some("text")
        && let Some(text) = value
            .get("part")
            .and_then(|part| part.get("text"))
            .and_then(Value::as_str)
    {
        append_text_fragment(&mut parsed.final_output, text);
        events.push(ModelStreamEvent::text_delta(text.to_string()));
    }
    if value.get("type").and_then(Value::as_str) == Some("reasoning")
        && let Some(text) = value
            .get("part")
            .and_then(|part| part.get("text"))
            .and_then(Value::as_str)
    {
        events.push(ModelStreamEvent::reasoning_summary_delta(text.to_string()));
    }
    if value.get("type").and_then(Value::as_str) == Some("tool_use")
        && let Some(name) = value
            .get("part")
            .and_then(|part| part.get("tool"))
            .and_then(Value::as_str)
    {
        events.push(ModelStreamEvent::tool_observed(
            name,
            observation_detail(&value),
        ));
    }
    if value.get("type").and_then(Value::as_str) == Some("step_finish")
        && let Some(tokens) = value.get("part").and_then(|part| part.get("tokens"))
    {
        let input_tokens = tokens
            .get("input")
            .and_then(Value::as_u64)
            .and_then(to_u32)
            .unwrap_or(0);
        let output_tokens = tokens
            .get("output")
            .and_then(Value::as_u64)
            .and_then(to_u32)
            .unwrap_or(0);
        let total_tokens = tokens
            .get("total")
            .and_then(Value::as_u64)
            .and_then(to_u32)
            .unwrap_or_else(|| input_tokens.saturating_add(output_tokens));
        parsed.token_usage = Some(ModelTokenUsage {
            input_tokens,
            output_tokens,
            total_tokens,
            cached_input_tokens: None,
            reasoning_tokens: None,
        });
    }
    events
}

fn observation_detail(value: &Value) -> Value {
    let item = value.get("item").or_else(|| value.get("part"));
    let mut detail = serde_json::Map::new();
    for (key, source) in [
        ("event", value.get("type")),
        ("id", item.and_then(|item| item.get("id"))),
        ("type", item.and_then(|item| item.get("type"))),
        (
            "status",
            item.and_then(|item| item.get("state"))
                .and_then(|state| state.get("status"))
                .or_else(|| item.and_then(|item| item.get("status"))),
        ),
    ] {
        if let Some(text) = source.and_then(Value::as_str) {
            detail.insert(key.into(), Value::String(text.to_owned()));
        }
    }
    if let Some(code) = item
        .and_then(|item| item.get("exit_code"))
        .and_then(Value::as_i64)
    {
        detail.insert("exitCode".into(), Value::from(code));
    }
    Value::Object(detail)
}

fn parse_json_line(line: &str) -> Option<Value> {
    let line = line.trim();
    if line.is_empty() || !line.starts_with('{') {
        return None;
    }
    serde_json::from_str::<Value>(line).ok()
}

fn append_text_fragment(buffer: &mut Option<String>, fragment: &str) {
    if fragment.is_empty() {
        return;
    }
    match buffer {
        Some(current) => current.push_str(fragment),
        None => *buffer = Some(fragment.to_string()),
    }
}

fn codex_session_id(value: &Value) -> Option<String> {
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if event_type == "thread.started" || event_type == "session.started" {
        return first_non_empty_string(
            value,
            &[
                &["thread_id"],
                &["threadId"],
                &["session_id"],
                &["sessionId"],
                &["session", "id"],
                &["thread", "id"],
            ],
        );
    }
    first_non_empty_string(
        value,
        &[
            &["thread_id"],
            &["threadId"],
            &["session_id"],
            &["sessionId"],
        ],
    )
}

fn opencode_session_id(value: &Value) -> Option<String> {
    first_non_empty_string(
        value,
        &[
            &["sessionID"],
            &["sessionId"],
            &["session_id"],
            &["session", "id"],
            &["part", "sessionID"],
            &["part", "sessionId"],
            &["part", "session_id"],
        ],
    )
}

fn first_non_empty_string(value: &Value, candidates: &[&[&str]]) -> Option<String> {
    candidates.iter().find_map(|path| {
        value_at_path(value, path)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| value.to_string())
    })
}

fn value_at_path<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    Some(current)
}

fn to_u32(value: u64) -> Option<u32> {
    u32::try_from(value).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ExternalAgentEventKind as ModelStreamEventKind;

    #[test]
    fn opencode_line_emits_reasoning_and_text_stream_events() {
        let mut parsed = ParsedSystemAgentOutput::default();
        let reasoning = parse_stream_line(
            SystemAgentCliKind::OpenCode,
            r#"{"type":"reasoning","sessionID":"ses-123","part":{"text":"Thinking..."}}"#,
            &mut parsed,
        );
        assert_eq!(parsed.system_agent_session_id.as_deref(), Some("ses-123"));
        assert_eq!(reasoning.len(), 2);
        assert_eq!(reasoning[0].kind, ModelStreamEventKind::SessionObserved);
        assert_eq!(reasoning[1].kind, ModelStreamEventKind::ReasoningSummary);
        assert_eq!(reasoning[1].delta, "Thinking...");

        let text = parse_stream_line(
            SystemAgentCliKind::OpenCode,
            r#"{"type":"text","part":{"text":"hello"}}"#,
            &mut parsed,
        );
        assert_eq!(text.len(), 1);
        assert_eq!(text[0].kind, ModelStreamEventKind::Text);
        assert_eq!(text[0].delta, "hello");
        assert_eq!(parsed.final_output.as_deref(), Some("hello"));

        let tool_use = parse_stream_line(
            SystemAgentCliKind::OpenCode,
            r#"{"type":"tool_use","part":{"tool":"glob","state":{"status":"completed"}}}"#,
            &mut parsed,
        );
        assert_eq!(tool_use.len(), 1);
        assert_eq!(tool_use[0].kind, ModelStreamEventKind::ToolObserved);
        assert_eq!(tool_use[0].delta, "glob");
        assert_eq!(tool_use[0].detail.as_ref().unwrap()["status"], "completed");
    }

    #[test]
    fn codex_line_records_usage_and_final_output() {
        let parsed = parse_complete_stdout(
            SystemAgentCliKind::Codex,
            r#"{"type":"thread.started","thread_id":"thr-1"}
{"type":"item.completed","item":{"type":"agent_message","text":"OK"}}
{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":3}}"#,
        );
        assert_eq!(parsed.system_agent_session_id.as_deref(), Some("thr-1"));
        assert_eq!(parsed.final_output.as_deref(), Some("OK"));
        let usage = parsed.token_usage.expect("usage should be parsed");
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 3);
        assert_eq!(usage.total_tokens, 13);
    }

    #[test]
    fn tool_and_approval_are_observations_not_reasoning() {
        let mut parsed = ParsedSystemAgentOutput::default();
        let tool = parse_stream_line(
            SystemAgentCliKind::Codex,
            r#"{"type":"item.started","item":{"id":"tool-1","type":"command_execution","command":"secret"}}"#,
            &mut parsed,
        );
        assert_eq!(tool[0].kind, ModelStreamEventKind::ToolObserved);
        assert_eq!(tool[0].detail.as_ref().unwrap()["id"], "tool-1");
        assert!(tool[0].detail.as_ref().unwrap().get("command").is_none());

        let approval = parse_stream_line(
            SystemAgentCliKind::OpenCode,
            r#"{"type":"permission.asked","part":{"id":"approval-1","prompt":"secret"}}"#,
            &mut parsed,
        );
        assert_eq!(approval[0].kind, ModelStreamEventKind::ApprovalObserved);
        assert!(approval[0].detail.as_ref().unwrap().get("prompt").is_none());
    }
}
