//! Trusted host operations reuse the command-tool process owner and readers.
use crate::tasks::{CommandRequest, CommandTaskManager};
use eliteforge_agent_core::ToolError;
use std::{collections::BTreeMap, path::PathBuf, time::Duration};
#[derive(Clone, Debug)]
pub struct CapturedCommand {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub env_remove: Vec<String>,
    pub timeout: Duration,
    pub max_output_bytes: usize,
}
pub async fn capture_command(spec: CapturedCommand) -> Result<serde_json::Value, ToolError> {
    if !spec.cwd.is_absolute() || !spec.cwd.is_dir() {
        return Err(ToolError::Failed(
            "configured working directory is unavailable".into(),
        ));
    }
    if spec.timeout.is_zero()
        || spec.max_output_bytes == 0
        || spec.max_output_bytes > 32 * 1024 * 1024
    {
        return Err(ToolError::Failed("invalid command capture limits".into()));
    }
    let manager = CommandTaskManager::default();
    struct Stop(CommandTaskManager);
    impl Drop for Stop {
        fn drop(&mut self) {
            self.0.stop_all();
        }
    }
    let _guard = Stop(manager.clone());
    let task = manager.start(
        CommandRequest {
            session_id: "host-capture".into(),
            command: spec.program,
            args: spec.args,
            cwd: Some(spec.cwd),
            env: spec.env,
            env_remove: spec.env_remove,
            stdin: None,
            interactive: false,
            tty: false,
            rows: 24,
            cols: 80,
        },
        spec.timeout,
        spec.max_output_bytes,
    )?;
    task.wait_for_terminal(spec.timeout.saturating_add(Duration::from_secs(5)))
        .await;
    let output = serde_json::to_value(task.view(0, 0, spec.max_output_bytes))
        .map_err(|error| ToolError::Failed(error.to_string()))?;
    manager.shutdown().await.map_err(ToolError::Failed)?;
    Ok(output)
}
/// Safe for synchronous adapters called inside or outside a Tokio runtime.
/// Only waiting is isolated; process control stays in the command-task manager.
pub fn capture_command_blocking(spec: CapturedCommand) -> Result<serde_json::Value, ToolError> {
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| ToolError::Failed(error.to_string()))?
            .block_on(capture_command(spec))
    })
    .join()
    .map_err(|_| ToolError::Failed("command capture thread failed".into()))?
}
