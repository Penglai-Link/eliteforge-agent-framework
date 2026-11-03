//! DeepSeek Harness transport; no host-specific types or credential contents.
use crate::*;
use serde_json::{Value, json};
use std::{collections::HashMap, path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
};
mod workspace;
pub use workspace::DshWorkspaceOptions;

#[derive(Debug, Clone, Default)]
pub struct DshCliOptions {
    pub reuse_stored_credentials: bool,
    /// Import only model-provider routes from this installed dsh profile.
    /// The active headless profile and home overrides retain route precedence.
    pub provider_profile: Option<String>,
    /// Optional native workspace grouping; effective task directories stay isolated.
    pub workspace: Option<DshWorkspaceOptions>,
}
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DshModelRecord {
    pub provider: String,
    pub model: String,
    pub display_name: String,
    pub context_window_tokens: Option<u32>,
    #[serde(default)]
    pub reasoning_efforts: Vec<String>,
}
#[derive(Debug, Clone, serde::Deserialize)]
pub struct DshModelProbe {
    pub models: Vec<DshModelRecord>,
    #[serde(default)]
    pub errors: Vec<String>,
}
fn failed(e: impl std::fmt::Display) -> TransportError {
    TransportError::Failed(e.to_string())
}
fn patches(options: &DshCliOptions) -> Vec<Value> {
    if options.reuse_stored_credentials {
        ["credentials", "authorization", "deepseek-account"]
            .into_iter()
            .map(|id| json!({"id": id, "disabled": false}))
            .collect()
    } else {
        Vec::new()
    }
}
// Provider configuration is profile-local in dsh. Import only llm-pi-ai routes;
// desktop UI, tools, permissions and its default model must never be mounted.
fn provider_overlay_at(
    home: &Path,
    profile: &str,
) -> Result<Option<serde_norway::Value>, TransportError> {
    use serde_norway::Value as Yaml;
    if profile.is_empty()
        || !profile
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(failed("invalid dsh provider profile name"));
    }
    let paths = [
        home.join("profiles").join(profile).join("cordis.patch.yml"),
        home.join("profiles/headless/cordis.patch.yml"),
        home.join("cordis.patch.yml"),
    ];
    let mut providers = serde_norway::Mapping::new();
    for path in paths {
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(failed(format!(
                    "cannot read dsh provider configuration: {error}"
                )));
            }
        };
        let rows: Yaml = serde_norway::from_str(&text)
            .map_err(|_| failed("invalid dsh provider profile YAML"))?;
        if rows.is_null() {
            continue;
        }
        let rows = rows
            .as_sequence()
            .ok_or_else(|| failed("dsh profile patch must be a list"))?;
        // Only the last config replacement for this row is effective in a layer.
        let config = rows
            .iter()
            .rfind(|row| row["id"].as_str() == Some("llm-pi-ai") && row.get("config").is_some());
        if let Some(row) = config {
            if row["disabled"].as_bool() == Some(true) {
                continue;
            }
            if let Some(routes) = row["config"]["providers"].as_mapping() {
                for (name, route) in routes {
                    providers.insert(name.clone(), route.clone());
                }
            }
        }
    }
    if providers.is_empty() {
        return Ok(None);
    }
    let mut config = serde_norway::Mapping::new();
    config.insert(Yaml::String("providers".into()), Yaml::Mapping(providers));
    let mut row = serde_norway::Mapping::new();
    row.insert(Yaml::String("id".into()), Yaml::String("llm-pi-ai".into()));
    row.insert(Yaml::String("config".into()), Yaml::Mapping(config));
    Ok(Some(Yaml::Mapping(row)))
}
fn write_overlay(
    path: &Path,
    options: &DshCliOptions,
    overlays: Vec<Value>,
) -> Result<(), TransportError> {
    let Some(profile) = options.provider_profile.as_deref() else {
        return std::fs::write(path, serde_json::to_vec(&overlays).map_err(failed)?)
            .map_err(failed);
    };
    let home = dsh_home()?;
    let mut rows = Vec::new();
    if let Some(row) = provider_overlay_at(&home, profile)? {
        rows.push(row);
    }
    rows.extend(
        overlays
            .into_iter()
            .map(serde_norway::to_value)
            .collect::<Result<Vec<_>, _>>()
            .map_err(failed)?,
    );
    std::fs::write(path, serde_norway::to_string(&rows).map_err(failed)?).map_err(failed)
}
fn dsh_home() -> Result<std::path::PathBuf, TransportError> {
    std::env::var_os("DSH_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                .map(|home| std::path::PathBuf::from(home).join(".dsh"))
        })
        .ok_or_else(|| failed("cannot resolve dsh home"))
}
fn prepare(command: &str, directory: Option<&str>, patch: &Path) -> Command {
    let mut cmd = Command::new(command);
    eliteforge_agent_official_plugin_tools_local::process_tree::prepare(&mut cmd);
    cmd.args(["--profile", "headless", "--patch"])
        .arg(patch)
        .env("NO_COLOR", "1");
    if let Some(dir) = directory {
        cmd.current_dir(dir);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd
}
struct Readers(Vec<tokio::task::AbortHandle>);
impl Drop for Readers {
    fn drop(&mut self) {
        for reader in &self.0 {
            reader.abort();
        }
    }
}
impl DshModelProbe {
    pub async fn discover(command: &str, options: DshCliOptions) -> Result<Self, TransportError> {
        tokio::time::timeout(Duration::from_secs(20), async {
            let dir = tempfile::tempdir().map_err(failed)?;
            let plugin = dir.path().join("eliteforge-dsh-model-catalog-probe.mjs");
            std::fs::write(&plugin, include_str!("../assets/dsh-model-catalog-probe.mjs")).map_err(failed)?;
            let mut overlay = patches(&options);
            overlay.extend([json!({"id":"headless-startup","disabled":true}), json!({"id":"headless-runner","disabled":true}),
                json!({"insert":[{"id":"eliteforge-dsh-model-catalog-probe","name":reqwest::Url::from_file_path(&plugin).map_err(|_| failed("invalid plugin path"))?.as_str()}]})]);
            let patch = dir.path().join("probe.json");
            write_overlay(&patch, &options, overlay)?;
            let mut cmd = prepare(command, Some(dir.path().to_str().ok_or_else(|| failed("invalid probe path"))?), &patch);
            cmd.stdin(Stdio::null());
            let child = cmd.spawn().map_err(failed)?;
            let _tree = eliteforge_agent_official_plugin_tools_local::process_tree::ProcessTree::attach(child.id().ok_or_else(|| failed("missing probe PID"))?).map_err(failed)?;
            let output = child.wait_with_output().await.map_err(failed)?;
            if !output.status.success() { return Err(failed(String::from_utf8_lossy(&output.stderr))); }
            serde_json::from_slice(&output.stdout).map_err(failed)
        }).await.map_err(|_| failed("dsh model discovery timed out after 20 seconds"))?
    }
    /// Safe from a synchronous host probe, including callers already on a Tokio runtime.
    pub fn discover_blocking(
        command: &str,
        options: DshCliOptions,
    ) -> Result<Self, TransportError> {
        let command = command.to_owned();
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(failed)?
                .block_on(Self::discover(&command, options))
        })
        .join()
        .map_err(|_| failed("dsh model probe panicked"))?
    }
}

pub(crate) async fn invoke(
    command: &str,
    directory: Option<&str>,
    options: &DshCliOptions,
    effort: Option<&str>,
    request: ExternalCliRequest,
    sink: ExternalAgentSink,
) -> Result<ExternalAgentResult, TransportError> {
    if request.prompt.trim().is_empty() {
        return Err(TransportError::PromptRequired);
    }
    let directory = directory
        .filter(|dir| Path::new(dir).is_absolute() && Path::new(dir).is_dir())
        .ok_or_else(|| {
            TransportError::NotStarted("configured working directory is unavailable".into())
        })?;
    let mut overlay = patches(options);
    if let Some(model) = &request.model {
        let (provider, model) = model
            .split_once('/')
            .filter(|(p, m)| !p.trim().is_empty() && !m.trim().is_empty())
            .ok_or_else(|| TransportError::NotStarted("dsh model must be provider/model".into()))?;
        let mut config = json!({"provider":provider,"model":model});
        if let Some(effort) = effort {
            config["reasoningEffort"] = json!(if effort == "none" { "off" } else { effort });
        }
        overlay.push(json!({"id":"agent-default-model","config":config}));
    }
    let dir = tempfile::tempdir().map_err(|e| TransportError::NotStarted(e.to_string()))?;
    let mut workspace = workspace::WorkspaceRun::prepare(
        command,
        directory,
        options,
        request.session_id.as_deref(),
        dir.path(),
    )
    .await?;
    if let Some(workspace) = &workspace {
        workspace.configure(&mut overlay, dir.path(), request.session_id.as_deref())?;
    }
    let patch = dir.path().join("run.json");
    write_overlay(&patch, options, overlay)?;
    let mut cmd = prepare(command, Some(directory), &patch);
    if let Some(workspace) = &workspace {
        workspace.command(&mut cmd);
    }
    cmd.arg("--json");
    if let Some(session) = &request.session_id {
        cmd.arg("--session-id").arg(session);
    }
    cmd.arg("-").stdin(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| TransportError::NotStarted(e.to_string()))?;
    let _tree = eliteforge_agent_official_plugin_tools_local::process_tree::ProcessTree::attach(
        child.id().ok_or_else(|| failed("missing dsh PID"))?,
    )
    .map_err(failed)?;
    let mut stdin = child.stdin.take().ok_or_else(|| failed("missing stdin"))?;
    let prompt = request.prompt;
    let writer = tokio::spawn(async move {
        stdin.write_all(prompt.as_bytes()).await?;
        stdin.shutdown().await
    });
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| failed("missing stderr"))?;
    let errors = tokio::spawn(async move {
        let mut text = String::new();
        stderr.read_to_string(&mut text).await.map(|_| text)
    });
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| failed("missing stdout"))?;
    let _readers = Readers(vec![writer.abort_handle(), errors.abort_handle()]);
    let mut lines = BufReader::new(stdout).lines();
    let mut parsed = Output::default();
    let setup_deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let line = if workspace
            .as_ref()
            .is_some_and(|workspace| !workspace.is_ready())
        {
            tokio::time::timeout_at(setup_deadline, lines.next_line())
                .await
                .map_err(|_| failed("dsh workspace setup timed out after 20 seconds"))?
        } else {
            lines.next_line().await
        };
        let Some(line) = line.map_err(failed)? else {
            break;
        };
        let event: Value = serde_json::from_str(&line).map_err(failed)?;
        if let Some(workspace) = &mut workspace {
            workspace.observe(&event, request.session_id.as_deref())?;
        }
        parsed.event(&event, &sink)?;
    }
    let status = child.wait().await.map_err(failed)?;
    writer.await.map_err(failed)?.map_err(failed)?;
    let stderr = errors.await.map_err(failed)?.map_err(failed)?;
    if !status.success() {
        return Err(failed(format!(
            "dsh exited {status}: {}",
            parsed.error.as_deref().unwrap_or(&stderr)
        )));
    }
    if let Some(error) = parsed.error {
        return Err(failed(error));
    }
    if !parsed.completed {
        return Err(failed("dsh did not report a completed turn"));
    }
    if let Some(workspace) = &workspace {
        workspace.finish(parsed.session.as_deref())?;
    }
    let output = parsed
        .text
        .ok_or_else(|| failed("dsh did not report final output"))?;
    for chunk in output.chars().collect::<Vec<_>>().chunks(4096) {
        sink(ExternalAgentEvent::text_delta(
            chunk.iter().collect::<String>(),
        ));
    }
    Ok(ExternalAgentResult {
        output,
        session_id: parsed.session.or(request.session_id),
        run_id: None,
        usage: if parsed.usage_complete {
            parsed.usage
        } else {
            None
        },
    })
}
#[derive(Default)]
struct Output {
    text: Option<String>,
    session: Option<String>,
    completed: bool,
    error: Option<String>,
    tools: HashMap<String, String>,
    usage: Option<ExternalAgentUsage>,
    usage_complete: bool,
    steps: usize,
}
impl Output {
    fn event(&mut self, v: &Value, sink: &ExternalAgentSink) -> Result<(), TransportError> {
        if !v.is_object() || v.get("type").and_then(Value::as_str).is_none() {
            return Err(failed("invalid dsh event envelope"));
        }
        let string = |key| {
            v.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        match string("type").as_str() {
            "session" => {
                if string("sessionId").is_empty() {
                    return Err(failed("invalid dsh session identity"));
                }
                self.session = Some(string("sessionId"));
                sink(ExternalAgentEvent::session_observed(string("sessionId")));
            }
            "final" => {
                self.text = Some(
                    v.get("text")
                        .and_then(Value::as_str)
                        .ok_or_else(|| failed("invalid dsh final"))?
                        .into(),
                );
            }
            "thinking" => sink(ExternalAgentEvent::reasoning_summary_delta(string("text"))),
            "error" => self.error = Some(string("message")),
            "tool_call" => {
                if string("callId").is_empty() || string("tool").is_empty() {
                    return Err(failed("invalid dsh tool call"));
                }
                let id = string("callId");
                let tool = string("tool");
                self.tools.insert(id.clone(), tool.clone());
                sink(ExternalAgentEvent::tool_observed(
                    tool,
                    json!({"id":id,"status":"running","truncated":v["truncated"]}),
                ));
            }
            "tool_result" => {
                if string("callId").is_empty()
                    || !matches!(string("status").as_str(), "error" | "completed")
                {
                    return Err(failed("invalid dsh tool result"));
                }
                let id = string("callId");
                let tool = self
                    .tools
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| "external".into());
                sink(ExternalAgentEvent::tool_observed(
                    tool,
                    json!({"id":id,"status":if string("status")=="error" {"failed"} else {"completed"},"truncated":v["truncated"]}),
                ));
            }
            "status" => {
                let phase = string("phase");
                sink(ExternalAgentEvent::status(&phase));
                if phase == "turn_end" {
                    self.completed = v["reason"]["kind"] == "completed";
                    if !self.completed {
                        self.error = Some(format!("dsh turn failed: {}", v["reason"]));
                    }
                }
                if phase == "step_end" {
                    self.add_usage(v.get("usage"));
                }
            }
            _ => {}
        }
        Ok(())
    }
    fn add_usage(&mut self, value: Option<&Value>) {
        if self.steps == 0 {
            self.usage_complete = true;
        }
        self.steps += 1;
        let Some(v) = value else {
            self.usage_complete = false;
            return;
        };
        let number = |key| {
            v.get(key)
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
        };
        let (Some(input), Some(output)) = (number("inputTokens"), number("outputTokens")) else {
            self.usage_complete = false;
            return;
        };
        let next = ExternalAgentUsage {
            input_tokens: input,
            output_tokens: output,
            total_tokens: number("totalTokens").unwrap_or(input.saturating_add(output)),
            cached_input_tokens: number("cacheReadTokens"),
            reasoning_tokens: number("reasoningTokens"),
        };
        self.usage = Some(match self.usage {
            None => next,
            Some(old) => ExternalAgentUsage {
                input_tokens: old.input_tokens.saturating_add(next.input_tokens),
                output_tokens: old.output_tokens.saturating_add(next.output_tokens),
                total_tokens: old.total_tokens.saturating_add(next.total_tokens),
                cached_input_tokens: old
                    .cached_input_tokens
                    .zip(next.cached_input_tokens)
                    .map(|(a, b)| a.saturating_add(b)),
                reasoning_tokens: old
                    .reasoning_tokens
                    .zip(next.reasoning_tokens)
                    .map(|(a, b)| a.saturating_add(b)),
            },
        });
    }
}

#[cfg(test)]
mod provider_profile_tests {
    use super::*;
    #[test]
    fn imports_provider_routes_without_desktop_plugins_and_preserves_overrides() {
        let home = tempfile::tempdir().unwrap();
        for name in ["desktop", "headless"] {
            std::fs::create_dir_all(home.path().join("profiles").join(name)).unwrap();
        }
        std::fs::write(
            home.path().join("profiles/desktop/cordis.patch.yml"),
            r#"
- id: ui-chat
  config: {privateUiSetting: should-not-copy}
- id: llm-pi-ai
  config:
    providers:
      custom: {apiKeyEnv: CUSTOM_KEY, api: openai-completions, models: [{id: model}]}
      overridden: {baseURL: desktop}
- id: agent-default-model
  config: {provider: overridden, model: default-should-not-copy}
"#,
        )
        .unwrap();
        std::fs::write(
            home.path().join("profiles/headless/cordis.patch.yml"),
            "- id: llm-pi-ai\n  config:\n    providers:\n      overridden: {baseURL: headless}\n",
        )
        .unwrap();
        let row = provider_overlay_at(home.path(), "desktop")
            .unwrap()
            .unwrap();
        assert_eq!(
            row["config"]["providers"]["custom"]["apiKeyEnv"].as_str(),
            Some("CUSTOM_KEY")
        );
        assert_eq!(
            row["config"]["providers"]["overridden"]["baseURL"].as_str(),
            Some("headless")
        );
        let serialized = serde_norway::to_string(&row).unwrap();
        assert!(!serialized.contains("privateUiSetting"));
        assert!(!serialized.contains("default-should-not-copy"));
        std::fs::write(
            home.path().join("cordis.patch.yml"),
            "- id: llm-pi-ai\n  config:\n    providers:\n      overridden: {baseURL: home}\n",
        )
        .unwrap();
        let row = provider_overlay_at(home.path(), "desktop")
            .unwrap()
            .unwrap();
        assert_eq!(
            row["config"]["providers"]["overridden"]["baseURL"].as_str(),
            Some("home")
        );
        assert!(provider_overlay_at(home.path(), "../desktop").is_err());
    }
}
