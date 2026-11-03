//! Native dsh workspace membership, separate from a session's effective cwd.
use super::{DshCliOptions, failed, patches, prepare, write_overlay};
use crate::TransportError;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{fs::File, path::Path, process::Stdio, time::Duration};

#[derive(Debug, Clone)]
pub struct DshWorkspaceOptions {
    /// Stable, absolute grouping directory, created on invocation if necessary.
    pub directory: std::path::PathBuf,
    pub title: String,
}

#[derive(Deserialize)]
struct Binding {
    version: u32,
    root: String,
    directory: String,
}
#[derive(Deserialize)]
struct Inspection {
    cwd: String,
    binding: Option<Binding>,
}

pub(super) struct WorkspaceRun {
    root: String,
    directory: String,
    title: String,
    // A process-independent lock protects the native JSON registry's read/write
    // interval. Drop releases it on cancellation, timeout and any setup failure.
    lock: Option<File>,
    ready_session: Option<String>,
}

fn path_string(path: &Path) -> Result<String, TransportError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| failed("dsh workspace directory is not UTF-8"))
}
fn plugin_url(path: &Path) -> Result<String, TransportError> {
    reqwest::Url::from_file_path(path)
        .map(|url| url.to_string())
        .map_err(|_| failed("invalid dsh workspace plugin path"))
}
fn write_assets(directory: &Path) -> Result<(), TransportError> {
    for (name, contents) in [
        (
            "dsh-workspace.mjs",
            include_str!("../../assets/dsh-workspace.mjs"),
        ),
        (
            "dsh-workspace-state.mjs",
            include_str!("../../assets/dsh-workspace-state.mjs"),
        ),
        (
            "dsh-workspace-policy.mjs",
            include_str!("../../assets/dsh-workspace-policy.mjs"),
        ),
        (
            "dsh-workspace-inspect.mjs",
            include_str!("../../assets/dsh-workspace-inspect.mjs"),
        ),
    ] {
        std::fs::write(directory.join(name), contents).map_err(failed)?;
    }
    Ok(())
}

async fn registry_lock() -> Result<File, TransportError> {
    let home = super::dsh_home()?;
    std::fs::create_dir_all(&home).map_err(failed)?;
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(home.join(".eliteforge-external-workspace.lock"))
        .map_err(failed)?;
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(std::fs::TryLockError::WouldBlock) => {
                    tokio::time::sleep(Duration::from_millis(50)).await
                }
                Err(std::fs::TryLockError::Error(error)) => return Err(failed(error)),
            }
        }
    })
    .await
    .map_err(|_| failed("dsh workspace registry is busy after 20 seconds"))?
}

impl WorkspaceRun {
    /// Old sessions keep their original native workspace. Grouped sessions must
    /// carry our durable binding; a shared header cwd alone proves no ownership.
    pub(super) async fn prepare(
        command: &str,
        directory: &str,
        options: &DshCliOptions,
        session: Option<&str>,
        assets: &Path,
    ) -> Result<Option<Self>, TransportError> {
        let Some(workspace) = &options.workspace else {
            return Ok(None);
        };
        if !workspace.directory.is_absolute() || workspace.title.trim().is_empty() {
            return Err(TransportError::NotStarted(
                "dsh workspace needs an absolute directory and a title".into(),
            ));
        }
        std::fs::create_dir_all(&workspace.directory).map_err(failed)?;
        let root = path_string(&workspace.directory.canonicalize().map_err(failed)?)?;
        let directory = path_string(&Path::new(directory).canonicalize().map_err(failed)?)?;
        let lock = registry_lock().await?;
        write_assets(assets)?;
        let run = Self {
            root,
            directory,
            title: workspace.title.clone(),
            lock: Some(lock),
            ready_session: None,
        };
        if let Some(session) = session {
            let inspected = run.inspect(command, options, session, assets).await?;
            match inspected.binding {
                Some(binding)
                    if binding.version == 1
                        && binding.root == run.root
                        && binding.directory == run.directory
                        && inspected.cwd == run.root => {}
                None if inspected.cwd == run.directory && inspected.cwd != run.root => {
                    return Ok(None);
                }
                _ => {
                    return Err(TransportError::NotStarted(
                        "dsh session belongs to a different task directory or workspace".into(),
                    ));
                }
            }
        }
        Ok(Some(run))
    }

    async fn inspect(
        &self,
        command: &str,
        options: &DshCliOptions,
        session: &str,
        assets: &Path,
    ) -> Result<Inspection, TransportError> {
        let mut overlay = patches(options);
        overlay.extend([
            json!({"id":"headless-startup","disabled":true}),
            json!({"id":"headless-runner","disabled":true}),
            json!({"insert":[{"id":"eliteforge-dsh-workspace-inspect","name":plugin_url(&assets.join("dsh-workspace-inspect.mjs"))?,"config":{"sessionId":session}}]}),
        ]);
        let patch = assets.join("inspect.json");
        write_overlay(&patch, options, overlay)?;
        tokio::time::timeout(Duration::from_secs(20), async {
            let mut cmd = prepare(command, Some(&self.directory), &patch);
            cmd.stdin(Stdio::null());
            let child = cmd.spawn().map_err(failed)?;
            let _tree =
                eliteforge_agent_official_plugin_tools_local::process_tree::ProcessTree::attach(
                    child
                        .id()
                        .ok_or_else(|| failed("missing dsh inspection PID"))?,
                )
                .map_err(failed)?;
            let output = child.wait_with_output().await.map_err(failed)?;
            if !output.status.success() {
                return Err(failed(format!(
                    "cannot inspect dsh session: {}",
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
            serde_json::from_slice(&output.stdout).map_err(failed)
        })
        .await
        .map_err(|_| failed("dsh session inspection timed out after 20 seconds"))?
    }

    pub(super) fn configure(
        &self,
        overlay: &mut Vec<Value>,
        assets: &Path,
        session: Option<&str>,
    ) -> Result<(), TransportError> {
        overlay.extend([
            json!({"id":"sandbox-policy","disabled":true}),
            json!({"insert":[
                {"id":"eliteforge-dsh-workspace-policy","name":plugin_url(&assets.join("dsh-workspace-policy.mjs"))?},
                {"id":"eliteforge-dsh-workspace-registry","name":"@deepseek-ai/dsh-workspace"},
                {"id":"eliteforge-dsh-workspace","name":plugin_url(&assets.join("dsh-workspace.mjs"))?,
                 "config":{"root":self.root,"directory":self.directory,"title":self.title,"sessionId":session}}
            ]}),
        ]);
        Ok(())
    }
    pub(super) fn command(&self, command: &mut tokio::process::Command) {
        command
            .current_dir(&self.root)
            .env("ELITEFORGE_DSH_WORKSPACE_ROOT", &self.root)
            .env("ELITEFORGE_DSH_WORKING_DIRECTORY", &self.directory);
    }
    pub(super) fn is_ready(&self) -> bool {
        self.ready_session.is_some()
    }
    pub(super) fn observe(
        &mut self,
        value: &Value,
        expected_session: Option<&str>,
    ) -> Result<(), TransportError> {
        if value["type"] != "eliteforge_workspace_ready" {
            return Ok(());
        }
        let session = value["sessionId"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| failed("invalid dsh workspace session identity"))?;
        if self.ready_session.is_some()
            || value["root"] != self.root
            || value["directory"] != self.directory
            || value["title"] != self.title
            || expected_session.is_some_and(|expected| expected != session)
        {
            return Err(failed("invalid dsh workspace binding acknowledgment"));
        }
        self.ready_session = Some(session.to_owned());
        // Membership is durable. Model execution does not hold the registry lock.
        self.lock.take();
        Ok(())
    }
    pub(super) fn finish(&self, session: Option<&str>) -> Result<(), TransportError> {
        if self.ready_session.as_deref().is_none() || self.ready_session.as_deref() != session {
            return Err(failed(
                "dsh did not confirm workspace and task directory binding",
            ));
        }
        Ok(())
    }
}
