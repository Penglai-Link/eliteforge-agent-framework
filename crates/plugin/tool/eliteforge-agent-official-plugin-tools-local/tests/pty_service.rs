#![cfg(unix)]
use eliteforge_agent_official_plugin_tools_local::{
    PtyCommand,
    pty::{PtyEvent, PtyService, PtySnapshot, PtyStart},
};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{Duration, Instant},
};
fn request(script: &str) -> PtyStart {
    PtyStart {
        owner: "workspace:user:chat".into(),
        invocation_id: None,
        command: script.into(),
        args: vec![],
        program: "/bin/sh".into(),
        program_args: vec!["-c".into(), script.into()],
        cwd: PathBuf::from("/tmp"),
        env: BTreeMap::new(),
    }
}
fn until(service: &PtyService, id: &str, predicate: impl Fn(&PtySnapshot) -> bool) -> PtySnapshot {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let value = service.read("workspace:user:chat", id, 0).unwrap();
        if predicate(&value) {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "terminal condition timed out: {}",
            value.output
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[test]
fn real_terminal_accepts_input_and_resize_and_enforces_owner() {
    let service = PtyService::default();
    let started = service
        .start(request(
            "stty -echo; printf READY; read answer; stty size; printf 'answer=%s' \"$answer\"",
        ))
        .unwrap();
    until(&service, &started.terminal_id, |value| {
        value.output.contains("READY")
    });
    assert!(service.read("other", &started.terminal_id, 0).is_err());
    assert!(service.write("other", &started.terminal_id, "bad").is_err());
    assert!(service.resize("other", &started.terminal_id, 1, 1).is_err());
    assert!(service.stop("other", &started.terminal_id).is_err());
    assert!(service.list("other").unwrap().is_empty());
    service
        .resize("workspace:user:chat", &started.terminal_id, 119, 41)
        .unwrap();
    service
        .write("workspace:user:chat", &started.terminal_id, "中文内容\n")
        .unwrap();
    let done = until(&service, &started.terminal_id, |value| {
        !value.running && value.output_closed
    });
    assert!(done.output.contains("41 119"), "{}", done.output);
    assert!(done.output.contains("answer=中文内容"), "{}", done.output);
    assert_eq!(done.exit_code, Some(0));
}
#[test]
fn dropping_event_receiver_does_not_stop_the_host_owned_process() {
    let service = PtyService::default();
    let mut command = PtyCommand::new("/bin/sh");
    command.args([
        "-c",
        "stty -echo; printf READY; read answer; printf survived",
    ]);
    command.cwd("/tmp");
    let (started, events) = service
        .start_stream("workspace:user:chat".into(), command, 80, 24)
        .unwrap();
    drop(events);
    until(&service, &started.terminal_id, |value| {
        value.output.contains("READY")
    });
    service
        .write("workspace:user:chat", &started.terminal_id, "ok\n")
        .unwrap();
    let done = until(&service, &started.terminal_id, |value| {
        !value.running && value.output_closed
    });
    assert!(done.output.contains("survived"));
}
#[test]
fn output_cursor_preserves_split_utf8_and_enforces_bounds() {
    let service = PtyService::default();
    let started = service
        .start(request(
            "stty -echo; printf '\\344'; read signal; printf '\\270\\255';",
        ))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let page = service
            .read("workspace:user:chat", &started.terminal_id, 0)
            .unwrap();
        assert_eq!(page.output, "");
        assert_eq!(page.next_cursor, 0);
        if service
            .read("workspace:user:chat", &started.terminal_id, 1)
            .is_ok()
        {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    service
        .write("workspace:user:chat", &started.terminal_id, "continue\n")
        .unwrap();
    let done = until(&service, &started.terminal_id, |value| {
        !value.running && value.output_closed
    });
    assert_eq!(done.output, "中");
    assert_eq!(done.next_cursor, 3);
    assert!(
        service
            .read("workspace:user:chat", &started.terminal_id, 4)
            .is_err()
    );
}
#[test]
fn close_rejects_new_sessions_and_kills_running_children() {
    let service = PtyService::default();
    let started = service
        .start(request("printf '%s' $$; exec sleep 60"))
        .unwrap();
    let running = until(&service, &started.terminal_id, |value| {
        !value.output.is_empty()
    });
    let pid: i32 = running.output.trim().parse().unwrap();
    let shared = service.clone();
    shared.close();
    assert!(service.start(request("exit 0")).is_err());
    assert!(
        service
            .read("workspace:user:chat", &started.terminal_id, 0)
            .is_err()
    );
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "process survived service close"
    );
}
#[tokio::test]
async fn stream_delivers_exact_bytes_then_exit_status() {
    let service = PtyService::default();
    let mut command = PtyCommand::new("/bin/sh");
    command.args(["-c", "printf '\\033[31m中文\\033[0m'; exit 7"]);
    command.cwd("/tmp");
    let (_, mut events) = service
        .start_stream("workspace:user:chat".into(), command, 80, 24)
        .unwrap();
    let mut output = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .unwrap()
            .unwrap()
        {
            PtyEvent::Output(bytes) => output.extend(bytes),
            PtyEvent::Exit(code) => {
                assert_eq!(code, Some(7));
                break;
            }
        }
    }
    assert_eq!(String::from_utf8(output).unwrap(), "\x1b[31m中文\x1b[0m");
}
