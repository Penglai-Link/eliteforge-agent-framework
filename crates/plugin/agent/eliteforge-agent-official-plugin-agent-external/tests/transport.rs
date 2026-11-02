#![cfg(unix)]
use eliteforge_agent_official_plugin_agent_external::*;
use std::{
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
};
fn script(dir: &tempfile::TempDir, body: &str) -> String {
    let path = dir.path().join("agent");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}
fn request() -> ExternalCliRequest {
    ExternalCliRequest {
        prompt: "hello world; $(touch INJECTED)".into(),
        model: Some("provider/model".into()),
        session_id: Some("session-1".into()),
    }
}

#[tokio::test]
async fn codex_resume_uses_process_directory_without_unsupported_cd_flag() {
    let dir = tempfile::tempdir().unwrap();
    let command = script(
        &dir,
        r#"printf '%s\n' "$@" > argv
pwd > cwd
echo '{"type":"item.completed","item":{"type":"agent_message","text":"done"}}'"#,
    );
    let cli = ExternalCli::new(
        ExternalCliKind::Codex,
        command,
        Some(dir.path().to_string_lossy().into()),
    );
    for session_id in [None, Some("session-1".to_string())] {
        let resumed = session_id.is_some();
        let mut input = request();
        input.session_id = session_id;
        cli.invoke(input, None).await.unwrap();
        let argv = std::fs::read_to_string(dir.path().join("argv")).unwrap();
        assert_eq!(argv.lines().any(|arg| arg == "--cd"), !resumed);
        assert_eq!(argv.lines().any(|arg| arg == "resume"), resumed);
        assert_eq!(argv.lines().any(|arg| arg == "session-1"), resumed);
        let cwd = std::fs::read_to_string(dir.path().join("cwd")).unwrap();
        assert_eq!(
            std::path::Path::new(cwd.trim()).canonicalize().unwrap(),
            dir.path().canonicalize().unwrap()
        );
    }
}
#[tokio::test]
async fn cli_arguments_are_literal_and_native_session_stream_is_preserved() {
    for kind in [ExternalCliKind::Codex, ExternalCliKind::OpenCode] {
        let dir = tempfile::tempdir().unwrap();
        let event = match kind {
            ExternalCliKind::Dsh => unreachable!(),
            ExternalCliKind::Codex => {
                r#"echo '{"type":"thread.started","thread_id":"session-2"}'
echo '{"type":"item.completed","item":{"type":"agent_message","text":"中文回复"}}'"#
            }
            ExternalCliKind::OpenCode => {
                r#"echo '{"type":"step_start","sessionID":"session-2"}'
echo '{"type":"text","part":{"text":"中文回复"}}'"#
            }
        };
        let command = script(&dir, &format!("printf '%s\\n' \"$@\" > argv\n{event}"));
        let events = Arc::new(Mutex::new(Vec::new()));
        let capture = events.clone();
        let result = ExternalCli::new(kind, command, Some(dir.path().to_string_lossy().into()))
            .invoke(
                request(),
                Some(Arc::new(move |event| capture.lock().unwrap().push(event))),
            )
            .await
            .unwrap();
        assert_eq!(result.output, "中文回复");
        assert_eq!(result.session_id.as_deref(), Some("session-2"));
        assert!(!dir.path().join("INJECTED").exists());
        let argv = std::fs::read_to_string(dir.path().join("argv")).unwrap();
        assert!(argv.lines().any(|line| line == request().prompt), "{argv}");
        assert!(argv.lines().any(|line| line == "session-1"));
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.kind == ExternalAgentEventKind::Text)
                .map(|event| event.delta.as_str())
                .collect::<String>(),
            "中文回复"
        );
    }
}
#[tokio::test]
async fn missing_directory_fails_before_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let command = script(&dir, "exit 91");
    let error = ExternalCli::new(ExternalCliKind::Codex, command, None)
        .invoke(request(), None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("working directory is required"));
    assert!(matches!(error, TransportError::NotStarted(_)));
}
#[tokio::test]
async fn cancelling_invocation_terminates_descendants() {
    let dir = tempfile::tempdir().unwrap();
    let command = script(&dir, "sleep 60 &\necho $! > child.pid\nwait");
    let cli = ExternalCli::new(
        ExternalCliKind::Codex,
        command,
        Some(dir.path().to_string_lossy().into()),
    );
    let task = tokio::spawn(async move { cli.invoke(request(), None).await });
    let pid = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(pid) = std::fs::read_to_string(dir.path().join("child.pid"))
                && !pid.trim().is_empty()
            {
                break pid.trim().to_owned();
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let status = std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &pid])
                .output()
                .unwrap();
            let status = String::from_utf8_lossy(&status.stdout);
            if status.trim().is_empty() || status.trim().starts_with('Z') {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn only_failures_before_process_spawn_are_known_unspent() {
    let dir = tempfile::tempdir().unwrap();
    for kind in [ExternalCliKind::Codex, ExternalCliKind::OpenCode] {
        let missing = dir
            .path()
            .join("no-such-command")
            .to_string_lossy()
            .into_owned();
        let error = ExternalCli::new(kind, missing, Some(dir.path().to_string_lossy().into()))
            .invoke(request(), None)
            .await
            .unwrap_err();
        assert!(matches!(error, TransportError::NotStarted(_)), "{error:?}");
        let command = script(&dir, "exit 91");
        let error = ExternalCli::new(
            kind,
            command.clone(),
            Some(
                dir.path()
                    .join("no-such-directory")
                    .to_string_lossy()
                    .into(),
            ),
        )
        .invoke(request(), None)
        .await
        .unwrap_err();
        assert!(matches!(error, TransportError::NotStarted(_)), "{error:?}");
        let error = ExternalCli::new(kind, command, Some(dir.path().to_string_lossy().into()))
            .invoke(request(), None)
            .await
            .unwrap_err();
        assert!(
            matches!(error, TransportError::Failed(_)),
            "a spawned process is ambiguous: {error:?}"
        );
    }
}

#[tokio::test]
async fn explicit_directory_override_controls_the_cli_process_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("task");
    std::fs::create_dir(&target).unwrap();
    let command = script(
        &dir,
        "pwd -P > actual-cwd\necho '{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"done\"}}'",
    );
    ExternalCli::new(
        ExternalCliKind::Codex,
        command,
        Some(dir.path().to_string_lossy().into_owned()),
    )
    .with_working_directory("task".into())
    .invoke(request(), None)
    .await
    .unwrap();
    let actual = std::fs::read_to_string(target.join("actual-cwd")).unwrap();
    assert_eq!(
        std::path::Path::new(actual.trim()),
        target.canonicalize().unwrap()
    );
    assert!(!dir.path().join("actual-cwd").exists());
}
