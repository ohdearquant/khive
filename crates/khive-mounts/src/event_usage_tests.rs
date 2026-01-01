//! The real catalog CAS and its conditional audit share event-row accounting.
use khive_runtime::{KhiveRuntime, Namespace};
use khive_storage::usage::{scope, UsageContext};
use khive_storage::{Event, EventFilter};
use khive_types::{EventKind, SubstrateKind};
use serde_json::json;

#[tokio::test]
async fn catalog_repin_usage_counts_the_actual_audit_row_but_not_a_lost_cas() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let sql = runtime.sql();
    super::initialize(&sql, "event-usage-fixture", &[])
        .await
        .unwrap();
    let store = runtime.events(&token).unwrap();
    let before = store.count_events(EventFilter::default()).await.unwrap();
    let audit = Event::new(
        "local",
        "mount.repin",
        EventKind::Audit,
        SubstrateKind::Event,
        "operator",
    );
    let committed_usage = UsageContext::new();
    scope(
        committed_usage.clone(),
        super::replace(&sql, "event-usage-fixture", 1, &[], &audit),
    )
    .await
    .unwrap();
    let after = store.count_events(EventFilter::default()).await.unwrap();
    assert_eq!(after - before, 1);
    assert_eq!(
        committed_usage.shipping_snapshot().unwrap()["event_rows"],
        json!(after - before)
    );
    assert!(store.get_event(audit.id).await.unwrap().is_some());
    assert_eq!(
        super::load(&sql, "event-usage-fixture")
            .await
            .unwrap()
            .unwrap()
            .0,
        2
    );

    let refused_audit = Event::new(
        "local",
        "mount.repin",
        EventKind::Audit,
        SubstrateKind::Event,
        "operator",
    );
    let refused_usage = UsageContext::new();
    assert!(scope(
        refused_usage.clone(),
        super::replace(&sql, "event-usage-fixture", 1, &[], &refused_audit)
    )
    .await
    .is_err());
    assert_eq!(
        store.count_events(EventFilter::default()).await.unwrap(),
        after
    );
    assert!(store.get_event(refused_audit.id).await.unwrap().is_none());
    assert!(refused_usage
        .shipping_snapshot()
        .unwrap()
        .get("event_rows")
        .is_none());
}
