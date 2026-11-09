//! Sudo credentials travel through a host-only service, never model arguments.
use async_trait::async_trait;
use eliteforge_agent_core::{RunContext, ToolError};
use std::sync::Arc;

/// Deliberately has no Debug or Serialize implementation.
pub struct SudoPassword(pub(crate) zeroize::Zeroizing<String>);
impl SudoPassword {
    pub fn new(value: String) -> Self {
        Self(zeroize::Zeroizing::new(value))
    }
    pub fn is_valid(&self) -> bool {
        !self.0.is_empty() && self.0.len() <= 4096 && !self.0.contains(['\r', '\n', '\0'])
    }
}

/// Implementations must use a private, non-persistent user input channel.
/// A None answer cancels the command. Never include the answer in events or errors.
#[async_trait]
pub trait SudoPasswordProvider: Send + Sync {
    async fn request(
        &self,
        command: &str,
        args: &[String],
        cwd: Option<&std::path::Path>,
        run: &RunContext,
    ) -> Result<Option<SudoPassword>, ToolError>;
}

#[derive(Clone)]
pub struct SudoPasswordProviderService(pub Arc<dyn SudoPasswordProvider>);

pub(crate) struct SudoInput {
    pub marker: String,
    pub command: String,
    pub args: Vec<String>,
    pub submitted: tokio::sync::Mutex<bool>,
}

impl SudoInput {
    pub async fn supply(
        &self,
        task: &crate::tasks::CommandTask,
        run: &RunContext,
    ) -> Result<(), ToolError> {
        let mut submitted = self.submitted.lock().await;
        if *submitted || !task.view(0, 0, 65536).stderr_contains(&self.marker) {
            return Ok(());
        }
        // A cancelled/dropped input request cannot be replayed against the process.
        *submitted = true;
        struct StopOnDrop<'a>(&'a crate::tasks::CommandTask, bool);
        impl Drop for StopOnDrop<'_> {
            fn drop(&mut self) {
                if !self.1 {
                    self.0.request_stop();
                }
            }
        }
        let mut guard = StopOnDrop(task, false);
        let provider = run
            .extensions
            .get::<SudoPasswordProviderService>()
            .ok_or_else(|| {
                ToolError::Failed("host does not provide private sudo password input".into())
            })?;
        let answer = tokio::select! {
            answer = provider.0.request(&self.command, &self.args, task.cwd(), run) => answer?,
            _ = run.cancellation.cancelled() => return Err(ToolError::Failed("sudo input cancelled".into())),
            _ = tokio::time::sleep(std::time::Duration::from_secs(45)) => return Err(ToolError::Failed("sudo password input expired".into())),
            _ = task.wait_for_terminal(std::time::Duration::from_secs(45)) => return Err(ToolError::Failed("sudo command finished or input expired".into())),
        };
        let Some(SudoPassword(password)) = answer else {
            return Err(ToolError::Failed("sudo password input cancelled".into()));
        };
        if password.is_empty() || password.len() > 4096 || password.contains(['\r', '\n', '\0']) {
            return Err(ToolError::Failed("invalid sudo password input".into()));
        }
        // Pipes do not echo. Closing after one line prevents an authentication retry
        // from hanging and prevents the elevated program from reading more input.
        task.write_secret(password).await?;
        guard.1 = true;
        Ok(())
    }
}
