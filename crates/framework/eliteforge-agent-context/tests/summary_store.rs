use eliteforge_agent_context::*;
use eliteforge_agent_framework::*;
use std::sync::Arc;

fn key() -> SummaryKey {
    SummaryKey {
        conversation: ConversationKey::new("s"),
        generation: "g1".into(),
        block_id: HISTORY_BLOCK.into(),
        policy_version: "v1".into(),
        configuration_digest: "b".repeat(64),
    }
}
fn summary(revision: u64) -> ConversationSummary {
    ConversationSummary {
        format_version: 1,
        source_revision: revision,
        covered_messages: revision - 1,
        source_digest: "a".repeat(64),
        summary: "prior facts".into(),
    }
}
async fn store_checks(store: &dyn SummaryStore) {
    let key = key();
    assert!(
        store
            .compare_exchange(&key, None, &summary(11))
            .await
            .unwrap()
    );
    let first = store.load(&key).await.unwrap().unwrap();
    assert!(
        !store
            .compare_exchange(&key, None, &summary(12))
            .await
            .unwrap()
    );
    assert!(
        !store
            .compare_exchange(&key, Some(&first.version), &summary(9))
            .await
            .unwrap(),
        "stale source cannot replace newer source"
    );
    assert!(
        store
            .compare_exchange(&key, Some(&first.version), &summary(13))
            .await
            .unwrap()
    );
    assert!(!store.delete(&key, &first.version).await.unwrap());
    assert!(
        !store
            .compare_exchange(&key, Some(&first.version), &summary(14))
            .await
            .unwrap()
    );
    let second = store.load(&key).await.unwrap().unwrap();
    assert_ne!(first.version, second.version);
    assert!(store.delete(&key, &second.version).await.unwrap());
    assert!(
        store
            .compare_exchange(&key, None, &summary(13))
            .await
            .unwrap()
    );
    assert!(
        !store
            .compare_exchange(&key, Some(&second.version), &summary(15))
            .await
            .unwrap(),
        "recreate does not reuse CAS identity"
    );
    let mut invalid = summary(17);
    invalid.format_version = 2;
    assert!(store.compare_exchange(&key, None, &invalid).await.is_err());
}
#[tokio::test]
async fn memory_and_sqlite_stores_enforce_versions_and_aba_protection() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSummaryStore::open(
        dir.path().join("summary.db"),
        SummaryStoreOptions::default(),
    )
    .await
    .unwrap();
    store_checks(&store).await;
    store.close().await.unwrap();
    assert!(store.load(&key()).await.is_err());
}
#[tokio::test]
async fn independent_sqlite_connections_cannot_both_win_the_same_revision() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("summary.db");
    let a = Arc::new(
        SqliteSummaryStore::open(&path, SummaryStoreOptions::default())
            .await
            .unwrap(),
    );
    let b = Arc::new(
        SqliteSummaryStore::open(&path, SummaryStoreOptions::default())
            .await
            .unwrap(),
    );
    let key = key();
    a.compare_exchange(&key, None, &summary(11)).await.unwrap();
    let version = a.load(&key).await.unwrap().unwrap().version;
    let value = summary(12);
    let (one, two) = tokio::join!(
        a.compare_exchange(&key, Some(&version), &value),
        b.compare_exchange(&key, Some(&version), &value)
    );
    assert_ne!(one.unwrap(), two.unwrap());
    a.close().await.unwrap();
    b.close().await.unwrap();
}
#[tokio::test]
async fn persisted_summary_backend_enforces_record_and_payload_limits() {
    let dir = tempfile::tempdir().unwrap();
    let options = SummaryStoreOptions {
        max_records: 1,
        max_record_bytes: 256,
        ..Default::default()
    };
    let store = SqliteSummaryStore::open(dir.path().join("summary.db"), options)
        .await
        .unwrap();
    let key = key();
    assert!(
        store
            .compare_exchange(&key, None, &summary(11))
            .await
            .unwrap()
    );
    let mut other = key.clone();
    other.generation = "g2".into();
    assert!(
        store
            .compare_exchange(&other, None, &summary(11))
            .await
            .is_err()
    );
    let mut oversized = summary(12);
    oversized.summary = "中".repeat(100);
    assert!(
        store
            .compare_exchange(&key, None, &oversized)
            .await
            .is_err()
    );
    assert_eq!(store.load(&key).await.unwrap().unwrap().value, summary(11));
    store.close().await.unwrap();
}
