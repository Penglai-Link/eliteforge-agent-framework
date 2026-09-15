use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use eliteforge_agent_capability_tools::{
    FileSystemToolsConfig, SystemCommandConfig, read_only_filesystem_pack, system_command_pack,
    write_filesystem_pack,
};
use eliteforge_agent_core::*;
use eliteforge_agent_framework::*;
use eliteforge_agent_test_support::TestConversationStore;
use serde_json::{Value, json};

fn builder(agent_loop: impl AgentLoop + 'static) -> AgentBuilder {
    AgentBuilder::new()
        .default_models(false)
        .plugin(AgentLoopPlugin::new(agent_loop))
        .plugin(ConversationStorePlugin::new(
            TestConversationStore::default(),
        ))
}

fn completed() -> RunResult {
    RunResult {
        output: String::new(),
        content: Content::default(),
        structured: None,
        status: RunStatus::Completed,
        interactions: Vec::new(),
        usage: Usage::default(),
        steps: 0,
        tool_calls: 0,
    }
}

struct InvokeToolLoop {
    call_name: &'static str,
    arguments: Value,
    output: Arc<Mutex<Option<Value>>>,
}

#[async_trait]
impl AgentLoop for InvokeToolLoop {
    async fn run(
        &self,
        _: &RunRequest,
        context: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        for provider in &services.tools {
            let lease = provider.lease(context).await?;
            let Some(binding) = lease
                .tools
                .iter()
                .find(|binding| binding.descriptor.call_name == self.call_name)
            else {
                continue;
            };
            let output = binding
                .executor
                .call(self.arguments.clone(), context)
                .await
                .map_err(|error| AgentError::Run(error.to_string()))?;
            *self
                .output
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = output.structured;
            return Ok(completed());
        }
        Err(AgentError::Run(format!(
            "tool `{}` was not provided by the selected packs",
            self.call_name
        )))
    }
}

async fn invoke(
    pack_builder: impl CapabilityPack + 'static,
    call_name: &'static str,
    arguments: Value,
) -> Value {
    let output = Arc::new(Mutex::new(None));
    let agent = builder(InvokeToolLoop {
        call_name,
        arguments,
        output: output.clone(),
    })
    .capability_pack(pack_builder)
    .build()
    .await
    .expect("selected capability pack should build");
    agent
        .run(RunRequest::new("filesystem-tools", "invoke"))
        .await
        .expect("tool loop should complete");
    assert!(agent.close().await.is_success());
    output
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
        .expect("tool output should be structured JSON")
}

async fn invoke_error(
    pack_builder: impl CapabilityPack + 'static,
    call_name: &'static str,
    arguments: Value,
) -> String {
    let output = Arc::new(Mutex::new(None));
    let agent = builder(InvokeToolLoop {
        call_name,
        arguments,
        output,
    })
    .capability_pack(pack_builder)
    .build()
    .await
    .expect("selected capability pack should build");
    let error = agent
        .run(RunRequest::new("filesystem-tools", "reject path escape"))
        .await
        .expect_err("filesystem tools must reject paths outside the configured root");
    assert!(agent.close().await.is_success());
    error.to_string()
}

#[tokio::test]
async fn readonly_and_write_filesystem_packs_operate_inside_the_configured_directory() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("notes.txt"),
        "first line\nneedle\nlast line\n",
    )
    .unwrap();
    let config = FileSystemToolsConfig::new(directory.path());

    let read = invoke(
        read_only_filesystem_pack(config.clone()),
        "fs_read_text",
        json!({"path":"notes.txt", "startLine":2}),
    )
    .await;
    assert_eq!(read["content"], json!("needle\nlast line"));

    let grep = invoke(
        read_only_filesystem_pack(config.clone()),
        "search_grep",
        json!({"pattern":"needle", "glob":["*.txt"]}),
    )
    .await;
    assert_eq!(grep["matches"][0]["path"], json!("notes.txt"));
    assert_eq!(grep["matches"][0]["line"], json!(2));

    let write = invoke(
        write_filesystem_pack(config.clone()),
        "fs_write_text",
        json!({"path":"nested/generated.txt", "content":"draft", "mode":"create"}),
    )
    .await;
    assert_eq!(write["bytesWritten"], json!(5));
    assert_eq!(
        std::fs::read_to_string(directory.path().join("nested/generated.txt")).unwrap(),
        "draft"
    );

    let patch = invoke(
        write_filesystem_pack(config),
        "fs_apply_patch",
        json!({"path":"nested/generated.txt", "action":"replace", "target":"draft", "content":"done"}),
    )
    .await;
    assert_eq!(patch["replacements"], json!(1));
    assert_eq!(
        std::fs::read_to_string(directory.path().join("nested/generated.txt")).unwrap(),
        "done"
    );
}

#[tokio::test]
async fn filesystem_packs_reject_parent_directory_escape() {
    let directory = tempfile::tempdir().unwrap();
    let error = invoke_error(
        read_only_filesystem_pack(FileSystemToolsConfig::new(directory.path())),
        "fs_read_text",
        json!({"path":"../outside.txt"}),
    )
    .await;
    assert!(error.contains("path must be relative to the configured filesystem root"));
}

#[cfg(unix)]
#[tokio::test]
async fn filesystem_packs_reject_symbolic_links() {
    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    std::os::unix::fs::symlink(outside.path(), directory.path().join("outside-link")).unwrap();
    let error = invoke_error(
        read_only_filesystem_pack(FileSystemToolsConfig::new(directory.path())),
        "fs_read_text",
        json!({"path":"outside-link"}),
    )
    .await;
    assert!(error.contains("symbolic links are not allowed"));
}

#[tokio::test]
async fn system_command_pack_uses_platform_native_command_runners() {
    #[cfg(windows)]
    let (command, args) = ("cmd", json!(["/C", "echo", "cross-platform"]));
    #[cfg(not(windows))]
    let (command, args) = ("sh", json!(["-c", "printf cross-platform"]));

    let output = invoke(
        system_command_pack(SystemCommandConfig::new()),
        "system_exec",
        json!({"command":command, "args":args}),
    )
    .await;
    assert_eq!(output["success"], json!(true));
    assert!(
        output["stdout"]
            .as_str()
            .expect("command output should be text")
            .contains("cross-platform")
    );
}

#[tokio::test]
async fn system_command_pack_can_restrict_executable_names() {
    let error = invoke_error(
        system_command_pack(SystemCommandConfig::restricted(["git"])),
        "system_exec",
        json!({"command":"not-allowed"}),
    )
    .await;
    assert!(error.contains("not permitted by this Agent configuration"));
}

#[tokio::test]
async fn system_command_pack_accepts_an_arbitrary_working_directory() {
    let directory = tempfile::tempdir().unwrap();
    #[cfg(windows)]
    let (command, args) = ("cmd", json!(["/C", "cd"]));
    #[cfg(not(windows))]
    let (command, args) = ("sh", json!(["-c", "pwd"]));

    let output = invoke(
        system_command_pack(SystemCommandConfig::new()),
        "system_exec",
        json!({
            "command":command,
            "args":args,
            "cwd":directory.path().to_string_lossy(),
        }),
    )
    .await;

    let actual = std::fs::canonicalize(
        output["stdout"]
            .as_str()
            .expect("command output should be text")
            .trim(),
    )
    .expect("command must run in the requested directory");
    assert_eq!(actual, std::fs::canonicalize(directory.path()).unwrap());
}
