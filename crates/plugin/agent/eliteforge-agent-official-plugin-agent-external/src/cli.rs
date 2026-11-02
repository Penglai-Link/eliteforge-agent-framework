use std::{path::Path, process::Stdio};

use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};

use crate::output::{ParsedSystemAgentOutput, parse_complete_stdout, parse_stream_line};
use crate::{
    ExternalAgentEvent as TransportEvent, ExternalAgentEventKind as TransportEventKind,
    ExternalAgentResult, ExternalAgentSink as TransportSink, ExternalCliKind as SystemAgentCliKind,
    ExternalCliRequest, TransportError,
};

const STREAM_DELTA_CHUNK_CHARS: usize = 72;

#[derive(Debug, Clone)]
pub struct ExternalCli {
    command_name: String,
    kind: SystemAgentCliKind,
    working_directory: Option<String>,
    reasoning_effort: Option<String>,
    fast_mode: Option<bool>,
    dsh_options: crate::DshCliOptions,
}

impl ExternalCli {
    pub fn new(
        kind: SystemAgentCliKind,
        command_name: impl Into<String>,
        working_directory: Option<String>,
    ) -> Self {
        Self {
            command_name: command_name.into(),
            kind,
            working_directory,
            reasoning_effort: None,
            fast_mode: None,
            dsh_options: crate::DshCliOptions::default(),
        }
    }

    pub fn with_working_directory(mut self, directory: String) -> Self {
        let directory = std::path::PathBuf::from(directory);
        let directory = if directory.is_relative() {
            self.working_directory.as_ref().map(|base| Path::new(base).join(&directory)).unwrap_or(directory)
        } else { directory };
        self.working_directory = Some(directory.to_string_lossy().into_owned());
        self
    }

    pub fn with_reasoning_effort(mut self, effort: Option<String>) -> Self {
        self.reasoning_effort = effort;
        self
    }

    pub fn with_dsh_options(mut self, options: crate::DshCliOptions) -> Self { self.dsh_options = options; self }

    pub fn with_fast_mode(mut self, enabled: bool) -> Self {
        self.fast_mode = Some(enabled);
        self
    }

    pub async fn invoke(
        &self,
        context: ExternalCliRequest,
        sink: Option<TransportSink>,
    ) -> Result<ExternalAgentResult, TransportError> {
        if self.kind == SystemAgentCliKind::Dsh {
            return crate::dsh::invoke(&self.command_name, self.working_directory.as_deref(), &self.dsh_options, self.reasoning_effort.as_deref(), context, sink.unwrap_or_else(|| std::sync::Arc::new(|_| {}))).await;
        }
        self.invoke_stream(context, sink.unwrap_or_else(|| std::sync::Arc::new(|_| {})))
            .await
    }

    fn build_command(&self, context: &ExternalCliRequest) -> Result<Command, TransportError> {
        let prompt = context.prompt.trim();
        if prompt.is_empty() {
            return Err(TransportError::PromptRequired);
        }
        let resume_session_id = context
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());

        let mut command = Command::new(&self.command_name);
        eliteforge_agent_official_plugin_tools_local::process_tree::prepare(&mut command);
        command.env("NO_COLOR", "1");
        let working_directory = self.working_directory.as_deref().ok_or_else(|| {
            TransportError::NotStarted("configured working directory is required".into())
        })?;
        if !Path::new(working_directory).is_absolute() || !Path::new(working_directory).is_dir() {
            return Err(TransportError::NotStarted(format!(
                "configured working directory `{working_directory}` is unavailable"
            )));
        }
        command.current_dir(working_directory);

        match self.kind {
            SystemAgentCliKind::Dsh => unreachable!("dsh uses its own protocol"),
            SystemAgentCliKind::Codex => {
                command.arg("exec");
                if resume_session_id.is_some() {
                    command.arg("resume");
                }
                command.arg("--skip-git-repo-check");
                command.arg("--json");
                if let Some(fast_mode) = self.fast_mode {
                    command.arg("--config");
                    command.arg(if fast_mode {
                        "service_tier=\"fast\""
                    } else {
                        "service_tier=\"default\""
                    });
                    command.arg("--config");
                    command.arg(format!("features.fast_mode={fast_mode}"));
                }
                if let Some(effort) = &self.reasoning_effort {
                    command.arg("--config");
                    command.arg(format!(
                        "model_reasoning_effort={}",
                        serde_json::to_string(effort).expect("string serialization")
                    ));
                }
                if let Some(model_name) =
                    normalized_value(Some(context.model.clone().unwrap_or_default()))
                {
                    command.arg("--model");
                    command.arg(model_name);
                }
                if resume_session_id.is_none() {
                    command.arg("--cd");
                    command.arg(working_directory);
                }
                if let Some(session_id) = resume_session_id {
                    command.arg(session_id);
                }
                command.arg(prompt);
            }
            SystemAgentCliKind::OpenCode => {
                command.arg("run");
                command.arg("--format");
                command.arg("json");
                command.arg("--thinking");
                if let Some(effort) = &self.reasoning_effort {
                    command.arg("--variant");
                    command.arg(effort);
                }
                if let Some(session_id) = resume_session_id {
                    command.arg("--session");
                    command.arg(session_id);
                }
                if let Some(working_directory) = self.working_directory.as_deref() {
                    command.arg("--dir");
                    command.arg(working_directory);
                }
                if context
                    .model
                    .as_ref()
                    .is_some_and(|model| model.contains('/'))
                {
                    command.arg("--model");
                    command.arg(context.model.as_deref().expect("selected model"));
                }
                command.arg(prompt);
            }
        }

        Ok(command)
    }

    async fn invoke_stream(
        &self,
        context: ExternalCliRequest,
        sink: TransportSink,
    ) -> Result<ExternalAgentResult, TransportError> {
        let mut command = self.build_command(&context)?;
        command.kill_on_drop(true);
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let mut child = command.spawn().map_err(|error| {
            TransportError::NotStarted(format!(
                "{} command failed to start: {error}",
                system_agent_name(self.kind)
            ))
        })?;

        let mut tree =
            eliteforge_agent_official_plugin_tools_local::process_tree::ProcessTree::attach(
                child.id().ok_or_else(|| {
                    TransportError::Failed("CLI child has no process identity".into())
                })?,
            )
            .map_err(|error| {
                TransportError::Failed(format!("CLI process ownership failed: {error}"))
            })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            TransportError::Failed(format!(
                "{} command did not expose stdout stream",
                system_agent_name(self.kind)
            ))
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            TransportError::Failed(format!(
                "{} command did not expose stderr stream",
                system_agent_name(self.kind)
            ))
        })?;

        let kind = self.kind;
        let sink_for_stdout = sink.clone();
        let stdout_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            let mut stream_output = CliStreamOutput::default();
            while let Some(line) = lines
                .next_line()
                .await
                .map_err(|error| format!("{kind:?} stdout read failed: {error}"))?
            {
                stream_output.stdout.push_str(&line);
                stream_output.stdout.push('\n');
                let events = parse_stream_line(kind, line.as_str(), &mut stream_output.parsed);
                for event in events {
                    if event.kind == TransportEventKind::Text {
                        stream_output.emitted_text_delta = true;
                    }
                    sink_for_stdout(event);
                }
            }
            Ok::<CliStreamOutput, String>(stream_output)
        });

        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            let mut stderr_buffer = String::new();
            while let Some(line) = lines
                .next_line()
                .await
                .map_err(|error| format!("{kind:?} stderr read failed: {error}"))?
            {
                stderr_buffer.push_str(&line);
                stderr_buffer.push('\n');
            }
            Ok::<String, String>(stderr_buffer)
        });

        // Reader tasks must not outlive a stopped/dropped Framework run.
        let _readers = AbortReaders(vec![stdout_task.abort_handle(), stderr_task.abort_handle()]);
        let status = child.wait().await.map_err(|error| {
            TransportError::Failed(format!(
                "{} command failed while waiting: {error}",
                system_agent_name(self.kind)
            ))
        })?;

        tree.terminate();
        let stream_output = stdout_task
            .await
            .map_err(|error| {
                TransportError::Failed(format!(
                    "{} stdout task join failed: {error}",
                    system_agent_name(self.kind)
                ))
            })?
            .map_err(TransportError::Failed)?;

        let stderr = stderr_task
            .await
            .map_err(|error| {
                TransportError::Failed(format!(
                    "{} stderr task join failed: {error}",
                    system_agent_name(self.kind)
                ))
            })?
            .map_err(TransportError::Failed)?;

        if !status.success() {
            return Err(TransportError::Failed(format_command_failure(
                self.kind,
                status.code(),
                &stream_output.stdout,
                &stderr,
            )));
        }

        if !stream_output.emitted_text_delta
            && let Some(output) = stream_output.parsed.final_output.clone()
            && !output.is_empty()
        {
            for chunk in split_stream_chunks(output.as_str(), STREAM_DELTA_CHUNK_CHARS) {
                sink(TransportEvent::text_delta(chunk));
            }
        }

        let parsed = if stream_output.parsed.final_output.is_none() {
            parse_complete_stdout(self.kind, &stream_output.stdout)
        } else {
            stream_output.parsed
        };
        self.run_result_from_parsed(context, parsed, stream_output.stdout)
    }

    fn run_result_from_parsed(
        &self,
        context: ExternalCliRequest,
        parsed: ParsedSystemAgentOutput,
        stdout: String,
    ) -> Result<ExternalAgentResult, TransportError> {
        let final_output = parsed
            .final_output
            .or_else(|| normalized_value(Some(stdout)))
            .ok_or_else(|| {
                TransportError::Failed(format!(
                    "{} command returned no assistant output",
                    system_agent_name(self.kind)
                ))
            })?;

        Ok(ExternalAgentResult {
            output: final_output,
            usage: parsed.token_usage,
            session_id: parsed.system_agent_session_id.or(context.session_id),
            run_id: None,
        })
    }
}

#[derive(Debug, Default)]
struct CliStreamOutput {
    emitted_text_delta: bool,
    parsed: ParsedSystemAgentOutput,
    stdout: String,
}

fn format_command_failure(
    kind: SystemAgentCliKind,
    status_code: Option<i32>,
    stdout: &str,
    stderr: &str,
) -> String {
    let diagnostic = normalized_value(Some(stderr.to_string()))
        .or_else(|| normalized_value(Some(stdout.to_string())))
        .unwrap_or_else(|| "no diagnostics available".into());
    let status = status_code
        .map(|code| code.to_string())
        .unwrap_or_else(|| "unknown".into());
    format!(
        "{} command failed with exit code {}: {}",
        system_agent_name(kind),
        status,
        diagnostic
    )
}

fn normalized_value(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn split_stream_chunks(value: &str, max_chars: usize) -> Vec<String> {
    if value.is_empty() || max_chars == 0 {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_chars = 0usize;
    for character in value.chars() {
        current.push(character);
        current_chars += 1;
        if current_chars >= max_chars {
            chunks.push(current.clone());
            current.clear();
            current_chars = 0;
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn system_agent_name(kind: SystemAgentCliKind) -> &'static str {
    match kind {
        SystemAgentCliKind::Codex => "codex",
        SystemAgentCliKind::OpenCode => "opencode",
        SystemAgentCliKind::Dsh => "dsh",
    }
}

struct AbortReaders(Vec<tokio::task::AbortHandle>);
impl Drop for AbortReaders {
    fn drop(&mut self) {
        for reader in &self.0 {
            reader.abort();
        }
    }
}
