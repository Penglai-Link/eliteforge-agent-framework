#![cfg(unix)]
use eliteforge_agent_official_plugin_agent_external::*;
use std::{
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
};
fn fixture(dir: &tempfile::TempDir, body: &str) -> String {
    let path = dir.path().join("dsh");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}
fn request() -> ExternalCliRequest {
    ExternalCliRequest {
        prompt: "hello; $(touch INJECTED)\n中文".into(),
        model: Some("provider/model/submodel".into()),
        session_id: None,
    }
}
#[tokio::test]
async fn dsh_uses_stdin_overlay_and_lossless_final() {
    let dir = tempfile::tempdir().unwrap();
    let command = fixture(
        &dir,
        r#"printf '%s\n' "$@" > argv
while [ "$1" != '--patch' ]; do shift; done
cp "$2" overlay
printf '%s' "$2" > patch-path
cat > input
echo '{"type":"session","sessionId":"session-1"}'
echo '{"type":"text","text":"intermediate truncated","truncated":true}'
echo '{"type":"thinking","text":"thinking"}'
echo '{"type":"tool_call","callId":"call-1","tool":"read"}'
echo '{"type":"tool_result","callId":"call-1","status":"completed"}'
echo '{"type":"status","phase":"step_end","usage":{"inputTokens":10,"outputTokens":2}}'
echo '{"type":"status","phase":"step_end","usage":{"inputTokens":20,"outputTokens":3}}'
echo '{"type":"status","phase":"turn_end","reason":{"kind":"completed"}}'
echo '{"type":"final","text":"完整答案"}'"#,
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let capture = events.clone();
    let result = ExternalCli::new(
        ExternalCliKind::Dsh,
        command,
        Some(dir.path().to_string_lossy().into_owned()),
    )
    .with_dsh_options(DshCliOptions {
        reuse_stored_credentials: true,
        provider_profile: None,
    })
    .with_reasoning_effort(Some("none".into()))
    .invoke(
        request(),
        Some(Arc::new(move |e| capture.lock().unwrap().push(e))),
    )
    .await
    .unwrap();
    assert_eq!(result.output, "完整答案");
    assert_eq!(result.session_id.as_deref(), Some("session-1"));
    assert_eq!(result.usage.unwrap().total_tokens, 35);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("input")).unwrap(),
        request().prompt
    );
    assert!(!dir.path().join("INJECTED").exists());
    let patch_path = std::fs::read_to_string(dir.path().join("patch-path")).unwrap();
    assert!(!std::path::Path::new(&patch_path).exists());
    let patch: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("overlay")).unwrap())
            .unwrap();
    assert_eq!(patch[3]["config"]["model"], "model/submodel");
    assert_eq!(patch[3]["config"]["reasoningEffort"], "off");
    let events = events.lock().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == ExternalAgentEventKind::Text)
            .map(|e| e.delta.as_str())
            .collect::<String>(),
        "完整答案"
    );
    let tools: Vec<_> = events
        .iter()
        .filter(|e| e.kind == ExternalAgentEventKind::ToolObserved)
        .collect();
    assert_eq!(tools.len(), 2);
    assert_eq!(
        tools[0].detail.as_ref().unwrap()["id"],
        tools[1].detail.as_ref().unwrap()["id"]
    );
}
#[tokio::test]
async fn dsh_protocol_failures_never_become_raw_answers() {
    for output in [
        "echo broken",
        "echo '{\"type\":\"final\",\"text\":\"bad\"}'",
        "echo '{\"type\":\"error\",\"message\":\"failed\"}'",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let command = fixture(&dir, &format!("cat >/dev/null\n{output}"));
        assert!(
            ExternalCli::new(
                ExternalCliKind::Dsh,
                command,
                Some(dir.path().to_string_lossy().into_owned())
            )
            .invoke(request(), None)
            .await
            .is_err()
        );
    }
}
#[tokio::test]
async fn probe_uses_isolated_overlay_and_no_task() {
    let dir = tempfile::tempdir().unwrap();
    let command=fixture(&dir,r#"while [ "$1" != '--patch' ]; do shift; done
cat "$2" > 'REPLACE'
echo '{"models":[{"provider":"test","model":"m","displayName":"Model","reasoningEfforts":["off","high"]}],"errors":["unavailable provider"]}'"#.replace("REPLACE",&dir.path().join("overlay").to_string_lossy()).as_str());
    let result = DshModelProbe::discover(&command, DshCliOptions::default())
        .await
        .unwrap();
    assert_eq!(result.models.len(), 1);
    assert_eq!(result.errors.len(), 1);
    let overlay = std::fs::read_to_string(dir.path().join("overlay")).unwrap();
    assert!(overlay.contains("headless-runner"));
    assert!(!overlay.contains("credentials"));
}
#[tokio::test]
#[ignore = "requires installed dsh, configured custom provider and saved credentials"]
async fn installed_dsh_directory_and_session_acceptance() {
    let command = std::env::var("DSH_COMMAND").unwrap_or_else(|_| {
        "/Applications/DeepSeek Harness.app/Contents/Resources/runtime/cli/bin/dsh".into()
    });
    let options = DshCliOptions {
        reuse_stored_credentials: true,
        provider_profile: Some("desktop".into()),
    };
    let catalog = DshModelProbe::discover(&command, options.clone())
        .await
        .unwrap();
    assert!(!catalog.models.is_empty(), "{:?}", catalog.errors);
    let selected_provider =
        std::env::var("DSH_ACCEPTANCE_PROVIDER").unwrap_or_else(|_| "alibaba-token-plan-1".into());
    let selected_model = std::env::var("DSH_ACCEPTANCE_MODEL").ok();
    let model = catalog
        .models
        .iter()
        .find(|model| {
            model.provider == selected_provider
                && selected_model
                    .as_ref()
                    .map_or(model.model == "qwen3.8-flash", |id| model.model == *id)
        })
        .or_else(|| {
            catalog
                .models
                .iter()
                .find(|model| model.provider == selected_provider)
        })
        .expect("configured custom provider should be discoverable");
    println!(
        "Discovered {} models; using {}/{}",
        catalog.models.len(),
        model.provider,
        model.model
    );
    let dir = tempfile::tempdir().unwrap();
    let cli = ExternalCli::new(
        ExternalCliKind::Dsh,
        command,
        Some(
            dir.path()
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        ),
    )
    .with_dsh_options(options);
    let first = cli
        .invoke(
            ExternalCliRequest {
                prompt: "Remember the word ORCHID. Reply only ORCHID. Do not use tools.".into(),
                model: Some(format!("{}/{}", model.provider, model.model)),
                session_id: None,
            },
            None,
        )
        .await
        .unwrap();
    assert!(first.output.contains("ORCHID"));
    let second=cli.invoke(ExternalCliRequest{prompt:"What word did I ask you to remember? Reply with that word only. Do not use tools.".into(),model:Some(format!("{}/{}",model.provider,model.model)),session_id:first.session_id.clone()},None).await.unwrap();
    assert_eq!(first.session_id, second.session_id);
    assert!(second.output.contains("ORCHID"));
}

#[tokio::test]
async fn resume_and_long_final_preserve_identity_and_content() {
    let dir = tempfile::tempdir().unwrap();
    let command = fixture(
        &dir,
        r#"printf '%s\n' "$@" > argv
cat >/dev/null
echo '{"type":"session","sessionId":"existing-session"}'
echo '{"type":"status","phase":"turn_end","reason":{"kind":"completed"}}'
printf '{"type":"final","text":"'
i=0; while [ "$i" -lt 40000 ]; do printf x; i=$((i+1)); done
printf '"}\n'"#,
    );
    let mut input = request();
    input.session_id = Some("existing-session".into());
    let result = ExternalCli::new(
        ExternalCliKind::Dsh,
        command,
        Some(dir.path().to_string_lossy().into_owned()),
    )
    .invoke(input, None)
    .await
    .unwrap();
    assert_eq!(result.output, "x".repeat(40000));
    assert!(result.usage.is_none());
    assert_eq!(result.session_id.as_deref(), Some("existing-session"));
    let argv = std::fs::read_to_string(dir.path().join("argv")).unwrap();
    assert!(argv.contains("--session-id\nexisting-session"));
}
#[tokio::test]
async fn cancellation_kills_children_and_removes_overlay() {
    let dir = tempfile::tempdir().unwrap();
    let command = fixture(
        &dir,
        r#"while [ "$1" != '--patch' ]; do shift; done
printf '%s' "$2" > patch-path
sleep 60 &
echo $! > child.pid
wait"#,
    );
    let cli = ExternalCli::new(
        ExternalCliKind::Dsh,
        command,
        Some(dir.path().to_string_lossy().into_owned()),
    );
    let task = tokio::spawn(async move { cli.invoke(request(), None).await });
    let pid = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(pid) = std::fs::read_to_string(dir.path().join("child.pid")) {
                break pid.trim().to_owned();
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let patch = std::fs::read_to_string(dir.path().join("patch-path")).unwrap();
    task.abort();
    let _ = task.await;
    assert!(!std::path::Path::new(&patch).exists());
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let output = std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &pid])
                .output()
                .unwrap();
            let state = String::from_utf8_lossy(&output.stdout);
            if state.trim().is_empty() || state.trim().starts_with('Z') {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn probe_timeout_removes_temporary_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let capture = dir.path().join("patch-path");
    let command = fixture(
        &dir,
        &format!(
            "while [ \"$1\" != '--patch' ]; do shift; done\nprintf '%s' \"$2\" > '{}'\nsleep 60",
            capture.display()
        ),
    );
    let started = std::time::Instant::now();
    let result = DshModelProbe::discover(&command, DshCliOptions::default()).await;
    assert!(result.unwrap_err().to_string().contains("timed out"));
    assert!(started.elapsed() < std::time::Duration::from_secs(25));
    let patch = std::fs::read_to_string(capture).unwrap();
    assert!(!std::path::Path::new(&patch).exists());
}
