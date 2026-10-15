use super::*;

/// #457: `brain.create_profile`, `brain.activate`, and `brain.bind` must
/// all survive a restart. Before the fix these mutated `ProfileRecord` /
/// `bindings` in memory only — `ensure_loaded` on a fresh `BrainPack`
/// replayed only `brain_event_log` since the last snapshot, so the
/// profile and binding vanished and `brain.resolve` fell back to
/// `balanced-recall-v1`.
///
/// This test uses ONE backing store (`rt`) and a genuinely FRESH
/// `BrainPack` instance (`pack2`) to simulate a restart, per the issue's
/// verification criteria — not a peek at the first pack's in-memory state.
#[tokio::test]
async fn profile_management_mutations_survive_restart() {
    let (pack, rt) = make_pack_with_actor("alice");
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    pack.dispatch(
        "brain.create_profile",
        json!({"name": "tenant-recall", "consumer_kind": "recall"}),
        &registry,
        &token,
    )
    .await
    .expect("brain.create_profile");

    pack.dispatch(
        "brain.activate",
        json!({"profile_id": "tenant-recall"}),
        &registry,
        &token,
    )
    .await
    .expect("brain.activate");

    pack.dispatch(
        "brain.bind",
        json!({
            "profile_id": "tenant-recall",
            "actor": "alice",
            "namespace": "local",
            "consumer_kind": "recall",
        }),
        &registry,
        &token,
    )
    .await
    .expect("brain.bind");

    // Simulate a restart: drop the first pack entirely and construct a
    // brand new one over the same backing store.
    drop(pack);
    let pack2 = crate::BrainPack::new(rt.clone());

    let profiles = pack2
        .dispatch("brain.profiles", json!({}), &registry, &token)
        .await
        .expect("brain.profiles on fresh pack");
    let tenant = profiles["profiles"]
        .as_array()
        .expect("profiles array")
        .iter()
        .find(|p| p["id"] == json!("tenant-recall"))
        .expect("tenant-recall profile must survive restart");
    assert_eq!(
        tenant["lifecycle"],
        json!("active"),
        "brain.activate must survive restart"
    );

    let bindings = pack2
        .dispatch(
            "brain.bindings",
            json!({"profile_id": "tenant-recall"}),
            &registry,
            &token,
        )
        .await
        .expect("brain.bindings on fresh pack");
    assert_eq!(
        bindings["count"],
        json!(1u64),
        "brain.bind must survive restart"
    );

    let resolved = pack2
        .dispatch(
            "brain.resolve",
            json!({"actor": "alice", "namespace": "local", "consumer_kind": "recall"}),
            &registry,
            &token,
        )
        .await
        .expect("brain.resolve on fresh pack");
    assert_eq!(
        resolved["resolved_profile_id"],
        json!("tenant-recall"),
        "resolve must use the durably-bound profile, not fall back to balanced-recall-v1"
    );
}

/// #457: `brain.reset` and `brain.unbind` must also survive a restart —
/// same durable-write helper, different mutations.
#[tokio::test]
async fn reset_and_unbind_survive_restart() {
    let (pack, rt) = make_pack_with_actor("bob");
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    // Advance balanced-recall-v1's posteriors, then reset — the reset
    // itself must be durable.
    pack.dispatch(
        "brain.feedback",
        json!({"target_id": target, "signal": "useful"}),
        &registry,
        &token,
    )
    .await
    .expect("brain.feedback");
    pack.dispatch("brain.reset", json!({}), &registry, &token)
        .await
        .expect("brain.reset");
    let epoch_after_reset = pack.snapshot().balanced_recall.exploration_epoch;

    pack.dispatch(
        "brain.bind",
        json!({
            "profile_id": "balanced-recall-v1",
            "actor": "bob",
            "namespace": "local",
            "consumer_kind": "recall",
        }),
        &registry,
        &token,
    )
    .await
    .expect("brain.bind");
    pack.dispatch("brain.unbind", json!({"actor": "bob"}), &registry, &token)
        .await
        .expect("brain.unbind");

    drop(pack);
    let pack2 = crate::BrainPack::new(rt.clone());

    let profile = pack2
        .dispatch(
            "brain.profile",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .expect("brain.profile on fresh pack");
    assert_eq!(
        profile["exploration_epoch"],
        json!(epoch_after_reset),
        "brain.reset's exploration_epoch bump must survive restart"
    );

    let bindings = pack2
        .dispatch("brain.bindings", json!({"actor": "bob"}), &registry, &token)
        .await
        .expect("brain.bindings on fresh pack");
    assert_eq!(
        bindings["count"],
        json!(0u64),
        "brain.unbind must survive restart — no bob binding should remain"
    );
}

/// #458 success path: `brain.feedback`'s posterior update must survive a
/// restart via the same durable-write helper used by #457, instead of the
/// old best-effort `persist_after_feedback` that ran after the in-memory
/// mutation and was logged (not surfaced) on failure.
#[tokio::test]
async fn feedback_posteriors_survive_restart() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    pack.dispatch(
        "brain.feedback",
        json!({"target_id": target, "signal": "useful"}),
        &registry,
        &token,
    )
    .await
    .expect("brain.feedback");

    let live_total = pack.snapshot().balanced_recall.total_events;

    drop(pack);
    let pack2 = crate::BrainPack::new(rt.clone());
    pack2
        .ensure_loaded(&token)
        .await
        .expect("ensure_loaded on fresh pack");

    assert_eq!(
        pack2.snapshot().balanced_recall.total_events,
        live_total,
        "brain.feedback's posterior update must survive restart"
    );
}

/// #458 fail-closed path: if the brain-specific persistence (event append
/// + snapshot upsert) fails, `brain.feedback` must return an error and
/// leave the in-memory state untouched — not log the failure as
/// non-fatal and still report `emitted: true`.
///
/// The failure is injected by dropping the real `brain_profile_snapshots`
/// table to simulate genuine schema drift, so this exercises the actual
/// SQL failure path rather than a test-only mock hook.
#[tokio::test]
async fn feedback_fails_closed_when_brain_persistence_fails() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    let before_us = chrono::Utc::now().timestamp_micros();
    let total_before = pack.snapshot().balanced_recall.total_events;

    {
        let sql = rt.sql();
        let mut writer = sql.writer().await.expect("writer");
        writer
            .execute_script("DROP TABLE brain_profile_snapshots;".to_string())
            .await
            .expect("drop brain_profile_snapshots to simulate schema drift");
    }

    let usage = khive_storage::usage::UsageContext::new();
    let err = khive_storage::usage::scope(
        usage.clone(),
        pack.dispatch(
            "brain.feedback",
            json!({"target_id": target, "signal": "useful"}),
            &registry,
            &token,
        ),
    )
    .await
    .expect_err("brain.feedback must fail when brain-specific persistence fails");
    let _ = err;
    assert!(
        usage.snapshot().get("event_rows").is_none(),
        "a rolled-back feedback append must not count an event row"
    );

    let total_after = pack.snapshot().balanced_recall.total_events;
    assert_eq!(
        total_after, total_before,
        "a failed brain-persistence write must leave in-memory state untouched"
    );

    // No brain_event_log row must have been committed for this call either
    // — the append and the (failing) snapshot upsert share one transaction.
    let sql = rt.sql();
    let replay = crate::persist::load_events_since(sql.as_ref(), "local", before_us)
        .await
        .expect("load_events_since must still work (brain_event_log untouched)");
    assert!(
        replay.events.is_empty(),
        "no brain_event_log row must be committed when the snapshot upsert fails"
    );
}
