use eliteforge_agent_official_plugin_tools_local::capture::{
    CapturedCommand, capture_command_with_cancel,
};
use std::{collections::BTreeMap, time::Duration};
fn command(root: &std::path::Path) -> CapturedCommand {
    CapturedCommand {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "touch started; sleep 0.3; touch late".into()],
        cwd: root.into(),
        env: BTreeMap::new(),
        env_remove: vec![],
        timeout: Duration::from_secs(5),
        max_output_bytes: 1024,
    }
}
#[tokio::test]
async fn capture_cancellation_stops_the_existing_process_owner_and_descendants() {
    let temp = tempfile::tempdir().unwrap();
    let (tx, mut rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(capture_command_with_cancel(
        command(temp.path()),
        async move {
            let _ = rx.changed().await;
        },
        tokio::time::Instant::now() + Duration::from_secs(3),
    ));
    tokio::time::timeout(Duration::from_secs(2), async {
        while !temp.path().join("started").exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    tx.send(true).unwrap();
    assert!(
        task.await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!temp.path().join("late").exists());
}
#[tokio::test]
async fn capture_deadline_stops_the_existing_process_owner_and_descendants() {
    let temp = tempfile::tempdir().unwrap();
    let error = capture_command_with_cancel(
        command(temp.path()),
        std::future::pending(),
        tokio::time::Instant::now() + Duration::from_millis(40),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("deadline exceeded"));
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!temp.path().join("late").exists());
}
