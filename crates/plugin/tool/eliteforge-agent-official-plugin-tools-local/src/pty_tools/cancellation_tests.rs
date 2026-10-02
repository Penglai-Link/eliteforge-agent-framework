use super::*;
fn tools(root: &std::path::Path) -> PtyTools {
    PtyTools::new(
        PtyService::default(),
        "owner".into(),
        SystemCommandConfig::new()
            .with_shell("/bin/sh")
            .with_directory_scope(
                crate::FileSystemToolsConfig::new(root).with_authorized_symlinks(true),
            ),
    )
}
#[tokio::test]
async fn abandoned_registered_start_is_stopped_by_the_native_owner() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let tools = tools(&root);
    let lifetime = TerminalInvocation::new(&tools);
    let (value, started) = tools
        .execute_owned(
            "terminal_start",
            json!({"command":"sleep 20"}),
            Some("new".into()),
        )
        .unwrap();
    lifetime.state.lock().unwrap().terminal_id = started;
    drop(lifetime);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if !tools
                .service
                .read("owner", value["terminalId"].as_str().unwrap(), 0)
                .unwrap()
                .running
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    tools.service.close();
}
#[tokio::test]
async fn abandoned_idempotent_replay_does_not_stop_an_accepted_terminal() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let tools = tools(&root);
    let (original, created) = tools
        .execute_owned(
            "terminal_start",
            json!({"command":"sleep 20"}),
            Some("same".into()),
        )
        .unwrap();
    assert!(created.is_some());
    let lifetime = TerminalInvocation::new(&tools);
    let (replay, created) = tools
        .execute_owned(
            "terminal_start",
            json!({"command":"sleep 20"}),
            Some("same".into()),
        )
        .unwrap();
    assert!(created.is_none());
    assert_eq!(original["terminalId"], replay["terminalId"]);
    lifetime.state.lock().unwrap().terminal_id = created;
    drop(lifetime);
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert!(
        tools
            .service
            .read("owner", original["terminalId"].as_str().unwrap(), 0)
            .unwrap()
            .running
    );
    tools.service.close();
}
