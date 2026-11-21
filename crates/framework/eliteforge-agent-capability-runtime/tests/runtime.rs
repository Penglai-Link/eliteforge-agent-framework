use async_trait::async_trait;
use eliteforge_agent_capability_runtime::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, io::Write, sync::Arc, time::Duration};

fn manifest(version: &str) -> Value {
    json!({"manifest_version":1,"id":"test.pack","name":"Test","version":version,"framework":"^1.0","pages":[{"id":"test.page","title":"Page","plugin":"test.plugin","entry":"ui/index.html","topics":["chart"]}]})
}
#[test]
fn portable_manifest_paths_and_mcp_transport_are_validated() {
    for path in ["../bad", "CON.txt", "a/LPT1.exe", "a/end.", "a/end "] {
        assert!(safe_relative(path).is_err());
    }
    assert!(safe_relative("资源/éé.md").is_ok());
    let mut value = manifest("1.0.0");
    value["mcp"] = json!([{"id":"mcp.local","transport":"stdio","program":"bin/mcp","args":[]}]);
    let parsed: CapabilityPackManifest = serde_json::from_value(value).unwrap();
    parsed.validate().unwrap();
}
#[tokio::test]
async fn a_separate_manager_cannot_delete_a_live_static_instance_version() {
    let directory = tempfile::tempdir().unwrap();
    let first = CapabilityPackManager::new(directory.path()).unwrap();
    let pack = first
        .install_bytes(&archive(manifest("1.0.0"), &[]), InstallPolicy::Development)
        .unwrap();
    let instance =
        BackendInstance::new(first, pack.clone(), "application", "config.1", Value::Null).unwrap();
    let second = CapabilityPackManager::new(directory.path()).unwrap();
    assert!(matches!(
        second.check_remove("test.pack", &pack.manifest.version),
        Err(RuntimeError::Busy(_))
    ));
    assert!(pack.path.exists());
    assert!(matches!(
        second.remove("test.pack", &pack.manifest.version),
        Err(RuntimeError::Busy(_))
    ));
    instance.shutdown().await.unwrap();
    second
        .check_remove("test.pack", &pack.manifest.version)
        .unwrap();
    assert!(pack.path.exists());
    second.remove("test.pack", &pack.manifest.version).unwrap();
}
fn archive(manifest: Value, extra: &[(&str, &[u8])]) -> Vec<u8> {
    let mut files = BTreeMap::from([
        (
            "manifest.json".to_owned(),
            serde_json::to_vec(&manifest).unwrap(),
        ),
        ("ui/index.html".into(), b"<p>test</p>".to_vec()),
    ]);
    for (name, bytes) in extra {
        files.insert(name.to_string(), bytes.to_vec());
    }
    let checksums: BTreeMap<_, _> = files
        .iter()
        .map(|(path, bytes)| (path.clone(), format!("{:x}", Sha256::digest(bytes))))
        .collect();
    files.insert(
        "checksums.json".into(),
        serde_json::to_vec(&checksums).unwrap(),
    );
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, bytes) in files {
        writer
            .start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(&bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}
#[test]
fn immutable_install_and_persistent_pin_enforce_version_identity() {
    let directory = tempfile::tempdir().unwrap();
    let manager = CapabilityPackManager::new(directory.path()).unwrap();
    let bytes = archive(manifest("1.0.0"), &[]);
    assert!(
        manager
            .install_bytes(&bytes, InstallPolicy::Signed)
            .is_err()
    );
    let pack = manager
        .install_bytes(&bytes, InstallPolicy::Development)
        .unwrap();
    assert_eq!(
        manager
            .install_bytes(&bytes, InstallPolicy::Development)
            .unwrap()
            .digest,
        pack.digest
    );
    assert!(
        manager
            .install_bytes(
                &archive(manifest("1.0.0"), &[("changed", b"data")]),
                InstallPolicy::Development
            )
            .is_err()
    );
    let requirement = PackVersionRequirement {
        package: pack.manifest.id.clone(),
        version: pack.manifest.version.clone(),
        digest: pack.digest.clone(),
    };
    manager
        .pin(DurablePackPin {
            id: "waiting".into(),
            scope: "agent.a".into(),
            versions: vec![requirement],
        })
        .unwrap();
    let reopened = CapabilityPackManager::new(directory.path()).unwrap();
    assert!(matches!(
        reopened.check_remove("test.pack", &pack.manifest.version),
        Err(RuntimeError::Busy(_))
    ));
    assert!(matches!(
        reopened.remove("test.pack", &pack.manifest.version),
        Err(RuntimeError::Busy(_))
    ));
    reopened.unpin("waiting").unwrap();
    reopened
        .remove("test.pack", &pack.manifest.version)
        .unwrap();
}
#[test]
fn tampering_and_archive_traversal_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let manager = CapabilityPackManager::new(directory.path()).unwrap();
    assert!(
        manager
            .install_bytes(
                &archive(manifest("1.0.0"), &[("../escape", b"bad")]),
                InstallPolicy::Development
            )
            .is_err()
    );
    let pack = manager
        .install_bytes(&archive(manifest("1.0.0"), &[]), InstallPolicy::Development)
        .unwrap();
    std::fs::write(pack.path.join("ui/index.html"), b"tampered").unwrap();
    assert!(manager.installed().is_err());
}
#[tokio::test]
async fn pages_pin_versions_and_reject_late_messages_without_starting_backend() {
    let directory = tempfile::tempdir().unwrap();
    let manager = CapabilityPackManager::new(directory.path()).unwrap();
    let pack = manager
        .install_bytes(&archive(manifest("1.0.0"), &[]), InstallPolicy::Development)
        .unwrap();
    let instance = BackendInstance::new(
        manager.clone(),
        pack.clone(),
        "agent.a",
        "config.1",
        json!({}),
    )
    .unwrap();
    let (page, mut receiver) = PageSession::new(
        instance.clone(),
        "test.page",
        PageContext {
            theme: "dark".into(),
            language: "zh-CN".into(),
            data: Value::Null,
        },
        Default::default(),
        Arc::new(NoPageHost),
    )
    .unwrap();
    let message = PageMessage {
        session: page.id().into(),
        instance: instance.identity.clone(),
        topic: "chart".into(),
        data: json!(1),
    };
    page.publish(message.clone()).await.unwrap();
    assert_eq!(receiver.recv().await.unwrap().data, json!(1));
    assert!(
        manager
            .usage()
            .blockers("test.pack")
            .iter()
            .any(|b| b.kind == "page")
    );
    assert!(
        !manager
            .usage()
            .blockers("test.pack")
            .iter()
            .any(|b| b.kind == "runtime")
    );
    page.close();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .is_none()
    );
    assert!(page.publish(message).await.is_err());
    drop(page);
    drop(instance);
    manager.remove("test.pack", &pack.manifest.version).unwrap();
}
struct Nested;
#[async_trait]
impl RpcHandler for Nested {
    async fn handle(&self, request: RpcRequest, peer: RpcPeer) -> Result<Value, RpcFault> {
        match request.method.as_str() {
            "nested" => peer
                .request_in_scope(
                    request.scope.as_deref(),
                    "host.next",
                    request.params,
                    Duration::from_secs(1),
                )
                .await
                .map_err(|e| RpcFault::new(e.to_string())),
            "slow" => {
                tokio::time::sleep(Duration::from_secs(10)).await;
                Ok(Value::Null)
            }
            _ => Ok(request.params),
        }
    }
}
fn pair() -> (RpcPeer, RpcPeer) {
    let (left, right) = tokio::io::duplex(4096);
    let (lr, lw) = tokio::io::split(left);
    let (rr, rw) = tokio::io::split(right);
    (
        RpcPeer::new(lr, lw, "a", Arc::new(RejectRequests)),
        RpcPeer::new(rr, rw, "b", Arc::new(Nested)),
    )
}
#[tokio::test]
async fn large_rpc_payloads_use_ordered_binary_chunks_and_leave_control_live() {
    let (host, backend) = pair();
    let payload = "x".repeat(MAX_JSON_FRAME + 4096);
    let response = host
        .request("echo", json!({"media":payload}), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(
        response["media"].as_str().unwrap().len(),
        MAX_JSON_FRAME + 4096
    );
    assert_eq!(
        host.request("echo", json!(42), Duration::from_secs(1))
            .await
            .unwrap(),
        json!(42)
    );
    assert!(
        host.send_binary("rpc:forged:0".into(), vec![1])
            .await
            .is_err()
    );
    host.close();
    backend.close();
}
#[tokio::test]
async fn cancellation_overtaking_a_request_prevents_late_dispatch() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Count(Arc<AtomicUsize>);
    #[async_trait]
    impl RpcHandler for Count {
        async fn handle(&self, _: RpcRequest, _: RpcPeer) -> Result<Value, RpcFault> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Value::Null)
        }
    }
    let count = Arc::new(AtomicUsize::new(0));
    let (mut writer, reader) = tokio::io::duplex(4096);
    let (read, write) = tokio::io::split(reader);
    let peer = RpcPeer::new(read, write, "receiver", Arc::new(Count(count.clone())));
    write_frame(
        &mut writer,
        &WireFrame::Json(json!({"jsonrpc":"2.0","method":"$/cancel","params":{"id":"sender:1"}})),
    )
    .await
    .unwrap();
    write_frame(
        &mut writer,
        &WireFrame::Json(json!({"jsonrpc":"2.0","id":"sender:1","method":"write","params":null})),
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(count.load(Ordering::SeqCst), 0);
    peer.close();
}
#[tokio::test]
async fn bidirectional_callbacks_keep_reading_while_request_waits() {
    let (host, backend) = pair();
    let mut scope = host.scope();
    let scope_id = scope.id().to_owned();
    let request = host.request_in_scope(
        Some(&scope_id),
        "nested",
        json!({"value":42}),
        Duration::from_secs(2),
    );
    tokio::pin!(request);
    let result = tokio::select! { call = scope.incoming.recv() => { let call = call.unwrap(); assert_eq!(call.request.method,"host.next"); call.respond(Ok(json!(43))).await.unwrap(); request.await.unwrap() }, result = &mut request => panic!("request completed without callback: {result:?}") };
    assert_eq!(result, json!(43));
    drop(scope);
    assert!(
        backend
            .request_in_scope(
                Some(&scope_id),
                "host.next",
                Value::Null,
                Duration::from_secs(1)
            )
            .await
            .is_err()
    );
    host.close();
    backend.close();
}
#[tokio::test]
async fn timeout_has_unknown_result_and_peer_close_is_prompt() {
    let (host, backend) = pair();
    assert!(matches!(
        host.request("slow", Value::Null, Duration::from_millis(20))
            .await,
        Err(RuntimeError::Indeterminate(_))
    ));
    assert_eq!(
        host.request("echo", json!(7), Duration::from_secs(1))
            .await
            .unwrap(),
        json!(7)
    );
    host.close();
    tokio::time::timeout(Duration::from_secs(1), backend.closed())
        .await
        .unwrap();
}

#[tokio::test]
async fn scoped_callback_observes_remote_cancellation_and_connection_shutdown() {
    let (host, backend) = pair();
    let mut scope = host.scope();
    let peer = backend.clone();
    let scope_id = scope.id().to_owned();
    let pending = tokio::spawn(async move {
        peer.request_in_scope(
            Some(&scope_id),
            "host.slow",
            Value::Null,
            Duration::from_secs(30),
        )
        .await
    });
    let incoming = scope.incoming.recv().await.unwrap();
    pending.abort();
    let _ = pending.await;
    tokio::time::timeout(Duration::from_secs(1), incoming.cancelled())
        .await
        .unwrap();
    incoming
        .respond(Err(RpcFault::new("cancelled")))
        .await
        .unwrap();
    let peer = backend.clone();
    let scope_id = scope.id().to_owned();
    let pending = tokio::spawn(async move {
        peer.request_in_scope(
            Some(&scope_id),
            "host.slow",
            Value::Null,
            Duration::from_secs(30),
        )
        .await
    });
    let incoming = scope.incoming.recv().await.unwrap();
    host.close();
    tokio::time::timeout(Duration::from_secs(1), incoming.cancelled())
        .await
        .unwrap();
    assert!(pending.await.unwrap().is_err());
    backend.close();
}

#[tokio::test]
async fn forwarded_faults_preserve_approval_data_and_indeterminate_outcome() {
    struct Faults;
    #[async_trait]
    impl RpcHandler for Faults {
        async fn handle(&self, request: RpcRequest, _: RpcPeer) -> Result<Value, RpcFault> {
            if request.method == "unknown" {
                Err(RpcFault::from(RuntimeError::Indeterminate(
                    "write may have committed".into(),
                )))
            } else {
                Err(RpcFault::from(RuntimeError::Fault(RpcFault {
                    code: -32010,
                    message: "approval required".into(),
                    data: Some(json!({"interaction":"id"})),
                })))
            }
        }
    }
    let (left, right) = tokio::io::duplex(4096);
    let (lr, lw) = tokio::io::split(left);
    let (rr, rw) = tokio::io::split(right);
    let host = RpcPeer::new(lr, lw, "host", Arc::new(RejectRequests));
    let backend = RpcPeer::new(rr, rw, "backend", Arc::new(Faults));
    assert!(
        matches!(host.request("unknown",Value::Null,Duration::from_secs(1)).await,Err(RuntimeError::Indeterminate(message)) if message.contains("committed"))
    );
    assert!(
        matches!(host.request("approval",Value::Null,Duration::from_secs(1)).await,Err(RuntimeError::Fault(fault)) if fault.code == -32010 && fault.data.as_ref().unwrap()["interaction"] == "id")
    );
    host.close();
    backend.close();
}
