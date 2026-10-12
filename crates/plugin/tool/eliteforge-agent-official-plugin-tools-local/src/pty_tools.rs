//! Official terminal tools. Hosts supply authority, never execution logic.
use crate::{
    SystemCommandConfig,
    pty::{PtyService, PtyStart},
};
use eliteforge_agent_core::{
    RunContext, Tool, ToolBinding, ToolCallIdentity, ToolDescriptor, ToolError, ToolOutput,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Clone)]
pub struct PtyTools {
    service: PtyService,
    owner: String,
    commands: SystemCommandConfig,
}
impl PtyTools {
    /// Owner and command scope must come from the host's authorization, never
    /// from model input. The service is shared with host terminal APIs.
    pub fn new(service: PtyService, owner: String, commands: SystemCommandConfig) -> Self {
        Self {
            service,
            owner,
            commands,
        }
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Start {
    command: Option<String>,
    #[serde(default)]
    args: Option<Vec<String>>,
    #[serde(default)]
    interactive: bool,
    cwd: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Read {
    terminal_id: String,
    #[serde(default)]
    cursor: u64,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Write {
    terminal_id: String,
    data: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Stop {
    terminal_id: String,
}
fn fail(error: impl ToString) -> ToolError {
    ToolError::Failed(error.to_string())
}
impl PtyTools {
    fn authorize_terminal(&self, id: &str) -> Result<(), ToolError> {
        let cwd = self.service.working_directory(&self.owner, id)?;
        self.commands.authorize_existing_cwd(Some(&cwd))
    }
    pub fn execute(
        &self,
        name: &str,
        input: Value,
        invocation_id: Option<String>,
    ) -> Result<Value, ToolError> {
        self.execute_owned(name, input, invocation_id)
            .map(|(value, _)| value)
    }
    fn execute_owned(
        &self,
        name: &str,
        input: Value,
        invocation_id: Option<String>,
    ) -> Result<(Value, Option<String>), ToolError> {
        let mut started = None;
        let owner = self.owner.clone();
        let value = match name {
            "terminal_start" => {
                let input: Start = serde_json::from_value(input).map_err(fail)?;
                let cwd = self
                    .commands
                    .resolve_cwd(input.cwd.as_deref())?
                    .ok_or_else(|| fail("working directory unavailable"))?
                    .canonicalize()
                    .map_err(fail)?;
                let (command, args, program, program_args) = if input.interactive {
                    if input.command.is_some() || input.args.is_some() {
                        return Err(fail(
                            "interactive terminal_start cannot include command or args",
                        ));
                    }
                    let shell = self.commands.interactive_shell()?;
                    let command = shell
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("sh")
                        .to_string();
                    (
                        command,
                        Vec::new(),
                        shell.to_string_lossy().into_owned(),
                        Vec::new(),
                    )
                } else {
                    let command = input
                        .command
                        .as_deref()
                        .ok_or_else(|| fail("command is required unless interactive is true"))?;
                    if is_bare_shell_invocation(command, input.args.as_deref()) {
                        return Err(fail("starting an empty shell requires interactive: true"));
                    }
                    let (program, program_args) = self
                        .commands
                        .terminal_invocation(command.trim().into(), input.args.clone())?;
                    (
                        command.trim().to_string(),
                        input.args.clone().unwrap_or_default(),
                        program,
                        program_args,
                    )
                };
                let (terminal, created) = self.service.start_owned(PtyStart {
                    owner,
                    invocation_id,
                    command,
                    args,
                    program,
                    program_args,
                    cwd,
                    env: input.env,
                })?;
                if created {
                    started = Some(terminal.terminal_id.clone());
                }
                serde_json::to_value(terminal).map_err(fail)?
            }
            "terminal_read" => {
                let input: Read = serde_json::from_value(input).map_err(fail)?;
                self.authorize_terminal(&input.terminal_id)?;
                serde_json::to_value(
                    self.service
                        .read(&owner, &input.terminal_id, input.cursor)?,
                )
                .map_err(fail)?
            }
            "terminal_write" => {
                let input: Write = serde_json::from_value(input).map_err(fail)?;
                self.authorize_terminal(&input.terminal_id)?;
                self.service
                    .write(&owner, &input.terminal_id, &input.data)?;
                json!({"terminalId":input.terminal_id,"written":true})
            }
            "terminal_stop" => {
                let input: Stop = serde_json::from_value(input).map_err(fail)?;
                self.service.stop(&owner, &input.terminal_id)?;
                json!({"terminalId":input.terminal_id,"stopped":true})
            }
            "terminal_list" => {
                if input != json!({}) {
                    return Err(fail("terminal_list takes no parameters"));
                }
                serde_json::to_value(
                    self.service
                        .list(&owner)?
                        .into_iter()
                        .filter(|terminal| {
                            self.commands
                                .authorize_existing_cwd(Some(&terminal.cwd))
                                .is_ok()
                        })
                        .collect::<Vec<_>>(),
                )
                .map_err(fail)?
            }
            _ => return Err(fail("unknown terminal operation")),
        };
        Ok((value, started))
    }
}

fn is_bare_shell_invocation(command: &str, args: Option<&[String]>) -> bool {
    let words = if let Some(args) = args {
        let mut words = vec![command.to_string()];
        words.extend(args.iter().cloned());
        words
    } else if let Ok(words) = shell_words::split(command) {
        words
    } else {
        return false;
    };
    let Some((program, arguments)) = words.split_first() else {
        return false;
    };
    matches!(
        std::path::Path::new(program)
            .file_name()
            .and_then(|name| name.to_str()),
        Some("bash" | "zsh" | "sh" | "dash" | "ksh" | "fish" | "pwsh" | "powershell")
    ) && !arguments.iter().any(|argument| {
        matches!(argument.as_str(), "--help" | "--version" | "-h" | "-V")
            || argument == "-c"
            || argument == "--command"
            || argument.starts_with("--command=")
            || argument.starts_with("-c")
            || !argument.starts_with('-')
    })
}
pub fn terminal_tool_catalog() -> Vec<ToolDescriptor> {
    [
        ("start", "Start an interactive shell or a long-running command in a host-authorized directory. Omit args for a shell command string, supply args for literal argv. An empty shell requires interactive:true. Returns a terminal ID for later reads, input and stop.", json!({
            "type":"object", "properties":{
                "command":{"type":"string","minLength":1}, "args":{"type":"array","items":{"type":"string"}},
                "interactive":{"type":"boolean"}, "cwd":{"type":"string"}, "env":{"type":"object","additionalProperties":{"type":"string"}}
            }, "oneOf":[
                {"required":["command"],"not":{"required":["interactive"],"properties":{"interactive":{"const":true}}}},
                {"required":["interactive"],"properties":{"interactive":{"const":true}},"not":{"anyOf":[{"required":["command"]},{"required":["args"]}]}}
            ], "additionalProperties":false
        })),
        ("read", "Read retained terminal output with a byte cursor. Sessions are scoped to the host-issued owner.", json!({
            "type":"object","required":["terminalId"],"properties":{"terminalId":{"type":"string"},"cursor":{"type":"integer","minimum":0}},"additionalProperties":false
        })),
        ("write", "Write input to an owned terminal.", json!({
            "type":"object","required":["terminalId","data"],"properties":{"terminalId":{"type":"string"},"data":{"type":"string","maxLength":65536}},"additionalProperties":false
        })),
        ("stop", "Stop an owned terminal and its managed child processes.", json!({
            "type":"object","required":["terminalId"],"properties":{"terminalId":{"type":"string"}},"additionalProperties":false
        })),
        ("list", "List terminal sessions belonging to the current owner.", json!({"type":"object","properties":{},"additionalProperties":false})),
    ].into_iter().map(|(name, description, schema)| ToolDescriptor::new(
        format!("framework.terminal.{name}"), format!("terminal_{name}"), description, schema
    ).with_operation_kind(if matches!(name, "read" | "list") {
        eliteforge_agent_core::ToolOperationKind::Query
    } else {
        eliteforge_agent_core::ToolOperationKind::Command
    })).collect()
}
// Scope of one awaited invocation, distinct from the persistent terminal owner.
#[derive(Default)]
struct TerminalInvocationState {
    abandoned: bool,
    accepted: bool,
    terminal_id: Option<String>,
}
struct TerminalInvocation {
    state: std::sync::Arc<std::sync::Mutex<TerminalInvocationState>>,
    service: PtyService,
    owner: String,
    runtime: tokio::runtime::Handle,
}
impl TerminalInvocation {
    fn new(tools: &PtyTools) -> Self {
        Self {
            state: Default::default(),
            service: tools.service.clone(),
            owner: tools.owner.clone(),
            runtime: tokio::runtime::Handle::current(),
        }
    }
    fn accept(&self) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .accepted = true;
    }
}
impl Drop for TerminalInvocation {
    fn drop(&mut self) {
        let terminal = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.accepted {
                return;
            }
            state.abandoned = true;
            state.terminal_id.clone()
        };
        if let Some(id) = terminal {
            // Stop uses the native service/process owner. It can block in kill/wait,
            // so a cancelled async caller must not perform it on the runtime thread.
            let service = self.service.clone();
            let owner = self.owner.clone();
            self.runtime.spawn_blocking(move || {
                let _ = service.stop(&owner, &id);
            });
        }
    }
}
struct TerminalBinding {
    name: String,
    tools: PtyTools,
}
#[async_trait::async_trait]
impl Tool for TerminalBinding {
    async fn call(&self, input: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let invocation_id = context
            .extensions
            .get::<ToolCallIdentity>()
            .map(|identity| identity.invocation_id.clone());
        let tools = self.tools.clone();
        let name = self.name.clone();
        let run = context.clone();
        let lifetime = TerminalInvocation::new(&tools);
        let invocation_state = lifetime.state.clone();
        let value = tokio::task::spawn_blocking(move || {
            if invocation_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .abandoned
            {
                return Err(fail("terminal invocation was abandoned before dispatch"));
            }
            run.check_cancelled().map_err(fail)?;
            if tokio::time::Instant::now() >= run.deadline {
                return Err(fail("terminal deadline exceeded"));
            }
            let (value, started) = tools.execute_owned(&name, input, invocation_id)?;
            let abandoned = {
                let mut state = invocation_state.lock().unwrap_or_else(|e| e.into_inner());
                state.terminal_id = started.clone();
                state.abandoned
            };
            if name == "terminal_start"
                && (abandoned || run.is_cancelled() || tokio::time::Instant::now() >= run.deadline)
            {
                if let Some(id) = started.as_deref() {
                    let _ = tools.service.stop(&tools.owner, id);
                }
                return Err(ToolError::Indeterminate(
                    "terminal start cancelled; process stop requested".into(),
                ));
            }
            Ok(value)
        })
        .await
        .map_err(fail)??;
        let output = crate::tools::json_output(value)?;
        lifetime.accept();
        Ok(output)
    }
}
pub(crate) fn bindings(
    service: PtyService,
    owner: String,
    commands: SystemCommandConfig,
) -> Vec<ToolBinding> {
    let tools = PtyTools::new(service, owner, commands);
    terminal_tool_catalog()
        .into_iter()
        .map(|descriptor| {
            let name = descriptor.call_name.clone();
            ToolBinding::new(
                descriptor,
                TerminalBinding {
                    name,
                    tools: tools.clone(),
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::is_bare_shell_invocation;

    #[test]
    fn empty_shell_commands_require_explicit_interactive_mode() {
        assert!(is_bare_shell_invocation("bash", None));
        assert!(is_bare_shell_invocation("/bin/zsh", Some(&[])));
        assert!(is_bare_shell_invocation(
            "bash",
            Some(&["--noprofile".into(), "--norc".into()])
        ));
        assert!(!is_bare_shell_invocation("bash -c 'npm run dev'", None));
        assert!(!is_bare_shell_invocation("bash --version", None));
        assert!(!is_bare_shell_invocation(
            "/bin/bash",
            Some(&["-c".into(), "npm run dev".into()])
        ));
    }
}

#[cfg(test)]
#[path = "pty_tools/cancellation_tests.rs"]
mod cancellation_tests;
