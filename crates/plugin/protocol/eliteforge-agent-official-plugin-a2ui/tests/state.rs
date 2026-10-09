use eliteforge_agent_core::*;
use eliteforge_agent_framework::{Framework, ReactPlugin};
use eliteforge_agent_official_plugin_a2ui::*;
use eliteforge_agent_official_plugin_storage_local::LocalStoragePlugin;
use serde_json::{Value, json};
async fn fixture(path: &std::path::Path) -> (Framework, std::sync::Arc<A2uiService>, String) {
    let artifacts = path.with_extension("artifacts");
    std::fs::create_dir_all(&artifacts).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&artifacts, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let f = Framework::builder()
        .default_models(false)
        .plugin(LocalStoragePlugin::new(
            path,
            path.with_extension("artifacts"),
        ))
        .plugin(ReactPlugin)
        .plugin(A2uiPlugin::new().unwrap())
        .build()
        .await
        .unwrap();
    let s = f
        .sessions()
        .unwrap()
        .create(SessionRecord::new("A2UI test"))
        .await
        .unwrap();
    let service = f.services().unwrap().get::<A2uiService>().unwrap();
    (f, service, s.id)
}
fn caps() -> A2uiClientCapabilities {
    A2uiClientCapabilities {
        version: VERSION.into(),
        catalog_ids: vec![AGENT_CATALOG_ID.into()],
    }
}
fn create(mode: &str) -> Value {
    json!({"mode":mode,"messages":[
        {"version":VERSION,"createSurface":{"surfaceId":"report","catalogId":AGENT_CATALOG_ID,"sendDataModel":true}},
        {"version":VERSION,"updateComponents":{"surfaceId":"report","components":[{"id":"root","component":"Button","child":"label","action":{"event":{"name":"search","context":{"query":{"path":"/query"}}}}},{"id":"label","component":"Text","text":"Search"}]}},
        {"version":VERSION,"updateDataModel":{"surfaceId":"report","value":{"query":"first"}}}
    ]})
}
fn action(s: &SurfaceRecord, id: &str) -> ActionInput {
    ActionInput {
        action_id: id.into(),
        surface_instance_id: s.surface_instance_id.clone(),
        expected_revision: s.revision,
        message: json!({"version":VERSION,"action":{"name":"search","surfaceId":"report","sourceComponentId":"root","timestamp":"2026-10-09T12:00:00Z","context":{"query":"second"}}}),
        metadata: json!({"a2uiClientDataModel":{"surfaces":{"report":{"query":"second"}}}}),
    }
}
#[tokio::test]
async fn durable_publish_and_action_receipts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    let (f, service, session) = fixture(&path).await;
    let event = service
        .publish(
            &session,
            "owner",
            "run",
            "invocation",
            &caps(),
            create("Reusable"),
        )
        .await
        .unwrap();
    assert_eq!(
        service
            .publish(
                &session,
                "owner",
                "run",
                "invocation",
                &caps(),
                create("Reusable")
            )
            .await
            .unwrap(),
        event
    );
    let state = service.state(&session).await.unwrap();
    assert_eq!(state.revision, 1);
    assert_eq!(state.surfaces.len(), 1);
    let input = action(&state.surfaces[0], "action");
    let receipt = service.admit_action(&session, input.clone()).await.unwrap();
    assert_eq!(
        service.admit_action(&session, input).await.unwrap().run_id,
        receipt.run_id
    );
    assert!(service.claim_action(&session, "action").await.unwrap());
    assert!(!service.claim_action(&session, "action").await.unwrap());
    service
        .complete_action(&session, "action", json!({"type":"RUN_FINISHED"}))
        .await
        .unwrap();
    f.close().await;
    drop(f);
    drop(service);
    let reopened = Framework::builder()
        .default_models(false)
        .plugin(LocalStoragePlugin::new(
            &path,
            path.with_extension("artifacts"),
        ))
        .plugin(ReactPlugin)
        .plugin(A2uiPlugin::new().unwrap())
        .build()
        .await
        .unwrap();
    let service = reopened.services().unwrap().get::<A2uiService>().unwrap();
    assert_eq!(
        service.state(&session).await.unwrap().surfaces[0]
            .snapshot
            .data_model["query"],
        "second"
    );
    assert!(
        service
            .action_receipt(&session, "action")
            .await
            .unwrap()
            .completion
            .is_some()
    );
    reopened.close().await;
}
#[tokio::test]
async fn lifecycle_ownership_revision_and_recreation() {
    let dir = tempfile::tempdir().unwrap();
    let (f, s, id) = fixture(&dir.path().join("s.db")).await;
    s.publish(&id, "owner", "run", "create", &caps(), create("Reusable"))
        .await
        .unwrap();
    let old = s.state(&id).await.unwrap().surfaces.remove(0);
    let update = json!({"mode":"Reusable","messages":[{"version":VERSION,"updateDataModel":{"surfaceId":"report","path":"/query","value":"updated"}}]});
    assert!(
        s.publish(&id, "other", "run", "bad-owner", &caps(), update.clone())
            .await
            .is_err()
    );
    s.publish(&id, "owner", "run", "update", &caps(), update)
        .await
        .unwrap();
    assert!(s.admit_action(&id, action(&old, "stale")).await.is_err());
    s.publish(&id,"owner","run","delete",&caps(),json!({"mode":"Reusable","messages":[{"version":VERSION,"deleteSurface":{"surfaceId":"report"}}]})).await.unwrap();
    s.publish(&id, "owner", "run", "recreate", &caps(), create("Reusable"))
        .await
        .unwrap();
    let new = s.state(&id).await.unwrap().surfaces.remove(0);
    assert_ne!(new.surface_instance_id, old.surface_instance_id);
    assert!(s.admit_action(&id, action(&old, "old-page")).await.is_err());
    let other = f
        .sessions()
        .unwrap()
        .create(SessionRecord::new("other"))
        .await
        .unwrap();
    assert!(
        s.admit_action(&other.id, action(&new, "cross-session"))
            .await
            .is_err()
    );
    f.close().await;
}
#[tokio::test]
async fn schema_display_and_capability_guards() {
    let dir = tempfile::tempdir().unwrap();
    let (f, s, id) = fixture(&dir.path().join("s.db")).await;
    let mut bad = create("Display");
    bad["messages"][1]["updateComponents"]["components"][0]["unexpected"] = json!(true);
    assert!(
        s.publish(&id, "owner", "run", "bad", &caps(), bad)
            .await
            .is_err()
    );
    assert_eq!(s.state(&id).await.unwrap().revision, 0);
    let mut unsupported = caps();
    unsupported.catalog_ids.clear();
    assert!(
        s.publish(
            &id,
            "owner",
            "run",
            "no-cap",
            &unsupported,
            create("Display")
        )
        .await
        .is_err()
    );
    s.publish(&id, "owner", "run", "display", &caps(), create("Display"))
        .await
        .unwrap();
    let surface = s.state(&id).await.unwrap().surfaces.remove(0);
    assert!(
        s.admit_action(&id, action(&surface, "read-only"))
            .await
            .is_err()
    );
    f.close().await;
}

#[tokio::test]
async fn bundled_skill_examples_pass_the_production_validator() {
    let dir = tempfile::tempdir().unwrap();
    let (f, service, session) = fixture(&dir.path().join("examples.db")).await;
    let examples: Value = serde_json::from_str(include_str!(
        "../skills/a2ui-interface/references/examples.json"
    ))
    .unwrap();
    for name in ["display", "reusable"] {
        service
            .publish(
                &session,
                "main-agent",
                "run",
                name,
                &caps(),
                examples[name].clone(),
            )
            .await
            .unwrap();
    }
    assert_eq!(service.state(&session).await.unwrap().surfaces.len(), 2);
    f.close().await;
}
