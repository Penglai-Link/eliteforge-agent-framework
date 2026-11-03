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
        ..Default::default()
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
    let group = dir.path().join("unused-workspace");
    let result = DshModelProbe::discover(
        &command,
        DshCliOptions {
            workspace: Some(DshWorkspaceOptions {
                directory: group.clone(),
                title: "Fixture".into(),
            }),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(result.models.len(), 1);
    assert_eq!(result.errors.len(), 1);
    let overlay = std::fs::read_to_string(dir.path().join("overlay")).unwrap();
    assert!(overlay.contains("headless-runner"));
    assert!(!overlay.contains("credentials"));
    assert!(!overlay.contains("dsh-workspace.mjs"));
    assert!(!group.exists());
}
#[tokio::test]
async fn workspace_binding_requires_a_matching_acknowledgment_and_cleans_resources() {
    for acknowledgment in ["valid", "wrong-directory", "missing"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let task = root.join("task");
        std::fs::create_dir(&task).unwrap();
        let ready = match acknowledgment {
            "missing" => String::new(),
            _ => format!(
                r#"printf '{{"type":"eliteforge_workspace_ready","sessionId":"grouped-session","title":"Fixture","root":"%s","directory":"%s"}}\n' "$ELITEFORGE_DSH_WORKSPACE_ROOT" "{}""#,
                if acknowledgment == "valid" {
                    task.to_str().unwrap()
                } else {
                    "/wrong/task"
                }
            ),
        };
        let command = fixture(
            &dir,
            &format!(
                r#"
while [ "$1" != '--patch' ]; do shift; done
printf '%s' "$2" > "{}/patch-path"
cp "$2" "{}/overlay"
pwd > "{}/cwd"
cat >/dev/null
{ready}
echo '{{"type":"session","sessionId":"grouped-session"}}'
echo '{{"type":"status","phase":"turn_end","reason":{{"kind":"completed"}}}}'
echo '{{"type":"final","text":"answer"}}'
"#,
                root.display(),
                root.display(),
                root.display()
            ),
        );
        let result = ExternalCli::new(
            ExternalCliKind::Dsh,
            command,
            Some(task.to_str().unwrap().into()),
        )
        .with_dsh_options(DshCliOptions {
            workspace: Some(DshWorkspaceOptions {
                directory: root.join("sessions"),
                title: "Fixture".into(),
            }),
            ..Default::default()
        })
        .invoke(request(), None)
        .await;
        assert_eq!(
            result.is_ok(),
            acknowledgment == "valid",
            "{acknowledgment}: {result:?}"
        );
        let cwd = std::fs::read_to_string(root.join("cwd")).unwrap();
        assert_eq!(cwd.trim(), root.join("sessions").to_str().unwrap());
        let patch = std::fs::read_to_string(root.join("patch-path")).unwrap();
        assert!(!std::path::Path::new(&patch).parent().unwrap().exists());
        let overlay = std::fs::read_to_string(root.join("overlay")).unwrap();
        assert!(overlay.contains("dsh-workspace-policy.mjs"));
        assert!(overlay.contains("dsh-workspace.mjs"));
    }
}

#[tokio::test]
#[ignore = "requires installed dsh, configured custom provider and saved credentials"]
async fn installed_dsh_directory_and_session_acceptance() {
    let dsh_home =
        std::env::var_os("DSH_HOME").expect("run installed acceptance with an isolated DSH_HOME");
    let command = std::env::var("DSH_COMMAND").unwrap_or_else(|_| {
        "/Applications/DeepSeek Harness.app/Contents/Resources/runtime/cli/bin/dsh".into()
    });
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let first_directory = root.join("project-a");
    let second_directory = root.join("project-b");
    std::fs::create_dir(&first_directory).unwrap();
    std::fs::create_dir(&second_directory).unwrap();
    std::fs::write(first_directory.join("workspace-marker.txt"), "ORCHID").unwrap();
    std::fs::write(second_directory.join("workspace-marker.txt"), "MAPLE").unwrap();
    for (directory, label) in [
        (&first_directory, "PROJECT_ALPHA"),
        (&second_directory, "PROJECT_BETA"),
    ] {
        std::fs::write(directory.join("AGENTS.md"), format!("When replying about workspace-marker.txt, include the label {label} in addition to the file contents.\n")).unwrap();
    }
    let options = DshCliOptions {
        reuse_stored_credentials: true,
        provider_profile: Some("desktop".into()),
        workspace: Some(DshWorkspaceOptions {
            directory: root.join("sessions"),
            title: "Tron harness".into(),
        }),
    };
    let catalog = DshModelProbe::discover(&command, options.clone())
        .await
        .unwrap();
    assert!(!root.join("sessions").exists());
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
    let cli = ExternalCli::new(
        ExternalCliKind::Dsh,
        &command,
        Some(first_directory.to_str().unwrap().to_owned()),
    )
    .with_dsh_options(options.clone());
    let other_cli = ExternalCli::new(
        ExternalCliKind::Dsh,
        &command,
        Some(second_directory.to_str().unwrap().into()),
    )
    .with_dsh_options(options);
    let (first, other) = tokio::join!(
        cli.invoke(
            ExternalCliRequest {
                prompt: "Read ./workspace-marker.txt using a file tool. Remember its contents. Follow the project instructions for your reply.".into(),
                model: Some(format!("{}/{}", model.provider, model.model)),
                session_id: None,
            },
            None,
        ),
        other_cli.invoke(ExternalCliRequest {
            prompt: "Read ./workspace-marker.txt using a file tool. Follow the project instructions for your reply.".into(),
            model: Some(format!("{}/{}", model.provider, model.model)), session_id: None,
        }, None)
    );
    let first = first.unwrap();
    let other = other.unwrap();
    assert!(first.output.contains("ORCHID"));
    assert!(first.output.contains("PROJECT_ALPHA"), "{}", first.output);
    assert!(other.output.contains("MAPLE"));
    assert!(other.output.contains("PROJECT_BETA"), "{}", other.output);
    assert_ne!(first.session_id, other.session_id);
    let second=cli.invoke(ExternalCliRequest{prompt:"What word did I ask you to remember? Reply with that word only. Do not use tools.".into(),model:Some(format!("{}/{}",model.provider,model.model)),session_id:first.session_id.clone()},None).await.unwrap();
    assert_eq!(first.session_id, second.session_id);
    assert!(second.output.contains("ORCHID"));
    let wrong = other_cli
        .invoke(
            ExternalCliRequest {
                prompt: "This must be rejected before any model request".into(),
                model: None,
                session_id: first.session_id.clone(),
            },
            None,
        )
        .await
        .unwrap_err();
    assert!(wrong.to_string().contains("different task directory"));
    // Independently verify the native registry, rather than trusting stdout alone.
    let registry: serde_json::Value = serde_json::from_slice(
        &std::fs::read(std::path::Path::new(&dsh_home).join("storages/workspace.json")).unwrap(),
    )
    .unwrap();
    let matching: Vec<_> = registry["tables"]["workspaces"]
        .as_object()
        .unwrap()
        .values()
        .filter(|workspace| workspace["path"].as_str() == root.join("sessions").to_str())
        .collect();
    assert_eq!(matching.len(), 1);
    assert_eq!(matching[0]["title"], "Tron harness");
    let members = matching[0]["sessionIds"].as_array().unwrap();
    for session in [&first.session_id, &other.session_id] {
        assert!(members.contains(&serde_json::json!(session.as_ref().unwrap())));
    }
    println!(
        "Shared workspace {:?}: two isolated directories, resumed {:?}, rejected cross-directory adoption",
        root.join("sessions"),
        second.session_id
    );
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
async fn legacy_session_keeps_its_original_directory_when_grouping_is_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let command = fixture(
        &dir,
        r#"while [ "$1" != '--patch' ]; do shift; done
case "$2" in */inspect.json)
    printf '{"cwd":"%s","binding":null}\n' "$PWD"
    exit 0;;
esac
pwd > cwd
cat >/dev/null
test -z "$ELITEFORGE_DSH_WORKSPACE_ROOT" || exit 2
echo '{"type":"session","sessionId":"legacy-session"}'
echo '{"type":"status","phase":"turn_end","reason":{"kind":"completed"}}'
echo '{"type":"final","text":"legacy answer"}'"#,
    );
    let mut input = request();
    input.session_id = Some("legacy-session".into());
    let result = ExternalCli::new(
        ExternalCliKind::Dsh,
        command,
        Some(root.to_str().unwrap().into()),
    )
    .with_dsh_options(DshCliOptions {
        workspace: Some(DshWorkspaceOptions {
            directory: root.join("sessions"),
            title: "Fixture".into(),
        }),
        ..Default::default()
    })
    .invoke(input, None)
    .await
    .unwrap();
    assert_eq!(result.session_id.as_deref(), Some("legacy-session"));
    assert_eq!(
        std::fs::read_to_string(root.join("cwd")).unwrap().trim(),
        root.to_str().unwrap()
    );
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
