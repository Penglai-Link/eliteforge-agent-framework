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
    match kind {
        SystemAgentCliKind::Codex => parse_codex_stdout_line(line, parsed),
        SystemAgentCliKind::OpenCode => parse_opencode_stdout_line(line, parsed),
    }
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
    let mut events = Vec::new();
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
        && let Some(summary) = opencode_tool_use_summary(&value)
    {
        events.push(ModelStreamEvent::reasoning_summary_delta(summary));
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

fn opencode_tool_use_summary(value: &Value) -> Option<String> {
    let part = value.get("part")?;
    let tool_name = part.get("tool").and_then(Value::as_str).map(str::trim)?;
    if tool_name.is_empty() {
        return None;
    }
    let status = part
        .get("state")
        .and_then(|state| state.get("status"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("running");

    let mut summary = format!("tool {tool_name} {status}");
    if let Some(title) = part
        .get("state")
        .and_then(|state| state.get("title"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        summary.push_str(": ");
        summary.push_str(title);
    }
    Some(summary)
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
        assert_eq!(reasoning.len(), 1);
        assert_eq!(reasoning[0].kind, ModelStreamEventKind::ReasoningSummary);
        assert_eq!(reasoning[0].delta, "Thinking...");

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
        assert_eq!(tool_use[0].kind, ModelStreamEventKind::ReasoningSummary);
        assert_eq!(tool_use[0].delta, "tool glob completed");
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
}
