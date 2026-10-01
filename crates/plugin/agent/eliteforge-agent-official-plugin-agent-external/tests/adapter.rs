#![cfg(unix)]
use std::{
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
};

use eliteforge_agent_core as fw;
use eliteforge_agent_official_plugin_agent_external::{
    CliExternalAgent, ExternalCli, ExternalCliKind,
};

#[derive(Default)]
struct Events(Mutex<Vec<fw::ExternalAgentEvent>>);

#[async_trait::async_trait]
impl fw::ExternalAgentEventSink for Events {
    async fn emit(&self, event: fw::ExternalAgentEvent) {
        self.0.lock().unwrap().push(event);
    }
}

fn request(session: Option<fw::ExternalSessionRef>) -> fw::ExternalAgentRequest {
    fw::ExternalAgentRequest {
        input: "hello".into(),
        model: None,
        session,
        extensions: fw::Extensions::default(),
    }
}

#[tokio::test]
async fn cli_is_external_agent_and_session_is_explicit() {
    let dir = tempfile::tempdir().unwrap();
    let command = dir.path().join("agent");
    std::fs::write(
        &command,
        "#!/bin/sh\necho '{\"type\":\"thread.started\",\"thread_id\":\"native-42\"}'\necho '{\"type\":\"item.started\",\"item\":{\"type\":\"command_execution\",\"id\":\"tool-1\"}}'\necho '{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"done\"}}'\necho '{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":2,\"output_tokens\":1}}'\n",
    ).unwrap();
    std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o755)).unwrap();
    let agent = CliExternalAgent::new(
        ExternalCli::new(
            ExternalCliKind::Codex,
            command.to_string_lossy(),
            Some(dir.path().to_string_lossy().into()),
        ),
        None,
    );
    let events = Arc::new(Events::default());
    let result = fw::ExternalAgent::run(&agent, request(None), events.as_ref())
        .await
        .unwrap();
    assert_eq!(result.content.text(), "done");
    assert_eq!(result.session.as_ref().unwrap().session_id, "native-42");
    assert_eq!(result.usage.input_tokens, 2);
    assert!(events.0.lock().unwrap().iter().any(|event| matches!(event, fw::ExternalAgentEvent::Tool { name, .. } if name == "command_execution")));
    assert!(events.0.lock().unwrap().iter().any(|event| matches!(event, fw::ExternalAgentEvent::Session(session) if session.session_id == "native-42")));

    // Continuation is supplied by the caller, never restored from Framework messages.
    fw::ExternalAgent::run(&agent, request(result.session), events.as_ref())
        .await
        .unwrap();
}

struct SlowEvents;
#[async_trait::async_trait]
impl fw::ExternalAgentEventSink for SlowEvents {
    async fn emit(&self, _event: fw::ExternalAgentEvent) {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}
#[tokio::test]
async fn fast_cli_and_slow_sink_fail_at_bounded_handoff_without_silent_event_loss() {
    let dir = tempfile::tempdir().unwrap();
    let command = dir.path().join("fast-agent");
    std::fs::write(
        &command,
        r#"#!/bin/sh
i=0
while [ "$i" -lt 3000 ]; do
  printf '{"type":"item.started","item":{"type":"command_execution","id":"tool-%s"}}\n' "$i"
  i=$((i + 1))
done
sleep 30
"#,
    )
    .unwrap();
    std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o755)).unwrap();
    let agent = CliExternalAgent::new(
        ExternalCli::new(
            ExternalCliKind::Codex,
            command.to_string_lossy(),
            Some(dir.path().to_string_lossy().into()),
        ),
        None,
    );
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        fw::ExternalAgent::run(&agent, request(None), &SlowEvents),
    )
    .await
    .unwrap();
    match result {
        Err(error) => assert!(error.to_string().contains("capacity exceeded"), "{error}"),
        Ok(_) => panic!("overflow must not return success"),
    }
}
