// Audit rows of `link` calls, including the aliased spelling. Included inside the
// inline `tests` module, whose fixtures it uses.
use super::*;

/// Dispatches `link(source, target, kind)` in its aliased spelling against a
/// handler that fails, optionally behind a gate that denies `link`, and
/// returns the one audit row the refused call leaves with the target it named.
async fn refused_aliased_link_audit_row(deny: bool) -> (uuid::Uuid, khive_storage::Event) {
    let target = uuid::Uuid::new_v4();
    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(LinkResultPack::err("target endpoint not found"));
    if deny {
        builder.with_gate(Arc::new(AuditCapturingGate {
            events: Default::default(),
            deny_verb: Some("link"),
        }));
    }
    builder.with_event_store(store.clone());
    builder.with_default_namespace("test-ns");
    let reg = builder.build().expect("registry builds");

    reg.dispatch(
        "link",
        serde_json::json!({
            "source": uuid::Uuid::new_v4(),
            "target": target,
            "kind": "depends_on",
        }),
    )
    .await
    .unwrap_err();

    let events = store.events.lock().unwrap();
    assert_eq!(events.len(), 1, "one audit row per refused call");
    (target, events[0].clone())
}

/// `link` accepts `target` for `target_id`. The audit row of a call that
/// errors is built from the submitted arguments, so it must read the alias.
#[tokio::test]
#[serial(config_ledger)]
async fn errored_aliased_link_audit_row_carries_the_target() {
    let (target, row) = refused_aliased_link_audit_row(false).await;
    assert_eq!(row.outcome, EventOutcome::Error);
    assert_eq!(row.target_id, Some(target));
}

/// The same holds for a call the gate denies.
#[tokio::test]
#[serial(config_ledger)]
async fn denied_aliased_link_audit_row_carries_the_target() {
    let (target, row) = refused_aliased_link_audit_row(true).await;
    assert_eq!(row.outcome, EventOutcome::Denied);
    assert_eq!(row.target_id, Some(target));
}

/// The alias is link's own: `target` is not `target_id` for other verbs.
#[tokio::test]
#[serial(config_ledger)]
async fn audit_row_target_ignores_a_target_key_on_other_verbs() {
    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_event_store(store.clone());
    builder.with_default_namespace("test-ns");
    let reg = builder.build().expect("registry builds");

    reg.dispatch(
        "create",
        serde_json::json!({"namespace": "test-ns", "target": uuid::Uuid::new_v4()}),
    )
    .await
    .unwrap();

    let events = store.events.lock().unwrap();
    assert_eq!(events.len(), 1, "the call must have been audited");
    assert!(events[0].target_id.is_none());
}

#[tokio::test]
#[serial(config_ledger)]
async fn link_audit_falls_back_to_v1_when_result_missing_edge_fields() {
    let store = Arc::new(MemoryEventStore::default());
    let target_arg = uuid::Uuid::new_v4();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(LinkResultPack::ok(serde_json::json!({"ok": true})));
    builder.with_event_store(store.clone());
    builder.with_default_namespace("test-ns");
    let reg = builder.build().expect("registry builds");

    reg.dispatch(
        "link",
        serde_json::json!({
            "source_id": uuid::Uuid::new_v4(),
            "target_id": target_arg,
            "relation": "depends_on",
        }),
    )
    .await
    .unwrap();

    let page = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    let ev = &page.items[0];
    assert_eq!(
        ev.payload_schema_version, 1,
        "an unparsable success result falls back to v1 rather than dropping the audit row"
    );
    assert_eq!(ev.outcome, EventOutcome::Success);
    assert_eq!(
        ev.target_id,
        Some(target_arg),
        "v1 fallback still extracts target_id from the raw dispatch args"
    );
    assert!(ev.payload.get("edge_id").is_none());
}
