//! Exercise the real remote InteractionStore adapter over an in-process RPC transport.
use super::*;
use async_trait::async_trait;
use eliteforge_agent_core::*;
use eliteforge_agent_test_support::storage_contracts::*;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::Write,
    sync::atomic::{AtomicUsize, Ordering},
};
struct BatchServer {
    calls: AtomicUsize,
    records: std::sync::Mutex<BTreeMap<RunId, InteractionRecord>>,
}
#[async_trait]
impl RpcHandler for BatchServer {
    async fn handle(&self, request: RpcRequest, _: RpcPeer) -> Result<Value, RpcFault> {
        assert_eq!(request.method, "extension.call");
        self.calls.fetch_add(1, Ordering::SeqCst);
        let operation = request.params["operation"].as_str().unwrap();
        let params = &request.params["params"];
        if operation == "publish_many" {
            let incoming: Vec<InteractionRecord> =
                serde_json::from_value(params["records"].clone()).unwrap();
            let mut records = self.records.lock().unwrap();
            for record in incoming {
                records.insert(record.request.id, record);
            }
            return Ok(Value::Null);
        }
        let ids: Vec<RunId> = serde_json::from_value(params["ids"].clone()).unwrap();
        let session = params["session_id"].as_str().unwrap();
        let records = self.records.lock().unwrap();
        let found = ids
            .iter()
            .map(|id| records.get(id).filter(|r| r.session_id == session).cloned())
            .collect::<Vec<_>>();
        match operation {
            "get_many" => {
                if found.iter().any(Option::is_none) {
                    Err(typed_fault(&StateError::NotFound, "missing interaction"))
                } else {
                    Ok(serde_json::to_value(
                        found.into_iter().map(Option::unwrap).collect::<Vec<_>>(),
                    )
                    .unwrap())
                }
            }
            "find_many" => Ok(serde_json::to_value(found).unwrap()),
            _ => Err(RpcFault::new(
                "single-item operations are deliberately unsupported by this contract peer",
            )),
        }
    }
}
fn archive() -> Vec<u8> {
    let manifest = json!({"manifest_version":1,"id":"test.storage-contract","name":"Storage contract","version":"1.0.0","framework":"^0.1"});
    let data = serde_json::to_vec(&manifest).unwrap();
    let checksums = serde_json::to_vec(&BTreeMap::from([(
        "manifest.json",
        format!("{:x}", Sha256::digest(&data)),
    )]))
    .unwrap();
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, bytes) in [("manifest.json", data), ("checksums.json", checksums)] {
        zip.start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(&bytes).unwrap();
    }
    zip.finish().unwrap().into_inner()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn remote_batches_have_one_rpc_round_trip_and_pass_concurrent_read_contract() {
    let directory = tempfile::tempdir().unwrap();
    let manager = CapabilityPackManager::new(directory.path()).unwrap();
    let pack = manager
        .install_bytes(&archive(), InstallPolicy::Development)
        .unwrap();
    let instance = BackendInstance::new(manager, pack, "test", "v1", Value::Null).unwrap();
    let server = Arc::new(BatchServer {
        calls: AtomicUsize::new(0),
        records: std::sync::Mutex::new(BTreeMap::new()),
    });
    let (left, right) = tokio::io::duplex(1024 * 1024);
    let (lr, lw) = tokio::io::split(left);
    let (rr, rw) = tokio::io::split(right);
    let client = RpcPeer::new(lr, lw, "client", Arc::new(RejectRequests));
    let backend = RpcPeer::new(rr, rw, "backend", server.clone());
    let (stop, _receiver) = mpsc::channel(1);
    *instance.session.lock().await = Some(ProcessSession {
        peer: client.clone(),
        stop,
        finished: Arc::new(Notify::new()),
        exited: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    });
    let remote = Arc::new(RemoteImplementation {
        instance: instance.clone(),
        declaration: ExtensionImplementation {
            id: "test.interactions".into(),
            extension: "framework.interactions".into(),
            contract: "^0.1".parse().unwrap(),
            dependencies: Vec::new(),
            metadata: Value::Null,
        },
    });
    let records = (1..=10_000)
        .map(|i| {
            let mut request =
                InteractionRequest::input("Choose", json!({"type":"string"})).unwrap();
            request.id = RunId::from_u128(i);
            InteractionRecord {
                session_id: "owner".into(),
                run_id: RunId::from_u128(1),
                request,
                tool_call: None,
                response: None,
                response_provenance: None,
                created_at_unix_ms: 1,
                answered_at_unix_ms: None,
            }
        })
        .collect::<Vec<_>>();
    let before = server.calls.load(Ordering::SeqCst);
    remote.publish_many(&records).await.unwrap();
    assert_query_budget(
        "remote publish batch",
        before,
        server.calls.load(Ordering::SeqCst),
        1,
    );
    for size in [1, 1000, 10_000] {
        let ids = records[..size]
            .iter()
            .rev()
            .map(|r| r.request.id)
            .collect::<Vec<_>>();
        let before = server.calls.load(Ordering::SeqCst);
        let found = remote.get_many("owner", &ids).await.unwrap();
        assert_query_budget(
            "remote batch lookup",
            before,
            server.calls.load(Ordering::SeqCst),
            1,
        );
        assert_eq!(found.iter().map(|r| r.request.id).collect::<Vec<_>>(), ids);
    }
    let ids = [records[0].request.id, RunId::nil(), records[0].request.id];
    let before = server.calls.load(Ordering::SeqCst);
    let found = remote.find_many("owner", &ids).await.unwrap();
    assert_query_budget(
        "remote optional batch",
        before,
        server.calls.load(Ordering::SeqCst),
        1,
    );
    assert_eq!(found.len(), 3);
    assert!(found[1].is_none());
    assert_eq!(found[0], found[2]);
    assert!(remote.get_many("foreign", &ids[..1]).await.is_err());
    let before = server.calls.load(Ordering::SeqCst);
    assert!(remote.get_many("owner", &[]).await.unwrap().is_empty());
    assert!(remote.find_many("owner", &[]).await.unwrap().is_empty());
    remote.publish_many(&[]).await.unwrap();
    assert_query_budget(
        "empty remote batch",
        before,
        server.calls.load(Ordering::SeqCst),
        0,
    );
    let reading = remote.clone();
    let before = server.calls.load(Ordering::SeqCst);
    let load = concurrent_reads(16, 40, move |worker, round| {
        let reading = reading.clone();
        async move {
            let ids = (1..=100)
                .map(|i| RunId::from_u128(i + (worker * 40 + round) as u128))
                .collect::<Vec<_>>();
            let values = reading.get_many("owner", &ids).await.unwrap();
            assert_eq!(values.iter().map(|r| r.request.id).collect::<Vec<_>>(), ids);
        }
    })
    .await;
    assert_query_budget(
        "remote concurrent batches",
        before,
        server.calls.load(Ordering::SeqCst),
        16 * 40,
    );
    eprintln!("{}", load.report("remote-interaction-batch-concurrent"));
    load.assert_p95("remote interaction batches", 1000.);
    client.close();
    backend.close();
    instance.session.lock().await.take();
}
