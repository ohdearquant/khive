//! End-to-end tests for `BrainPack` as a `DispatchHook` (issue #158).
//!
//! Per ADR-032, `BrainState` now holds a profile registry; the BalancedRecall
//! profile's `total_events` counter lives in `snapshot.balanced_recall.total_events`.
//! These tests verify the dispatch hook still drives the BalancedRecallFold.

use std::sync::Arc;

use khive_pack_brain::BrainPack;
use khive_pack_kg::KgPack;
use khive_runtime::{DispatchHook, KhiveRuntime, Namespace, PackRuntime, VerbRegistryBuilder};
use serde_json::json;

/// Promote `namespace` on `brain` via the production dispatch path.
///
/// Calls `brain.dispatch("brain.profiles", …)` which acquires the dispatch
/// gate and runs the full `ensure_loaded` path (snapshot + event-replay +
/// pending-signals drain).  The `_registry` passed here needs no brain
/// registration because the brain verb is routed entirely within the
/// `BrainPack::dispatch` impl.
async fn promote_namespace(brain: &BrainPack, rt: &KhiveRuntime, namespace: &str) {
    use khive_runtime::{Namespace, VerbRegistryBuilder};
    let registry = VerbRegistryBuilder::new()
        .build()
        .expect("minimal registry for promotion");
    let ns = Namespace::parse(namespace).expect("valid namespace string");
    let token = rt.authorize(ns).expect("authorize namespace token");
    brain
        .dispatch("brain.profiles", json!({}), &registry, &token)
        .await
        .expect("brain.profiles must succeed to promote namespace via production path");
}

/// Cold-path regression: the hook must update brain state even when no namespace
/// has been pre-activated.  Before the fix, `on_dispatch` returned early whenever
/// `active_namespace != event.namespace`, silently dropping the first (and all
/// subsequent) cold-namespace events.
///
/// This test passes NO prior activation call.
/// The registry's default namespace is "local"; the hook must create a cold bucket
/// for "local" and apply the signal there.
///
/// Round-3 strengthening: after firing the hook signal, the namespace is promoted
/// via the production `brain.dispatch("brain.profiles", …)` path.
/// The signal must survive into the active state.
#[tokio::test]
async fn dispatch_hook_fires_on_cold_namespace_no_prior_activation() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let brain = Arc::new(BrainPack::new(rt.clone()));
    // Deliberately do NOT activate any namespace — this is the cold path.

    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let hook: Arc<dyn DispatchHook> = brain.clone();
    builder.with_dispatch_hook(hook);
    let registry = builder.build().expect("registry builds");

    // Fire a real verb with the default "local" namespace.
    registry
        .dispatch(
            "create",
            json!({
                "kind": "entity",
                "name": "ColdHookProbe",
                "entity_kind": "concept"
            }),
        )
        .await
        .expect("create entity must succeed");

    // Before promotion: the signal must be in the cold pending queue.
    let cold_events = brain.cold_namespace_total_events("local");
    assert!(
        cold_events.is_some(),
        "cold-namespace 'local' pending queue must have been initialised by the hook"
    );
    assert_eq!(
        cold_events.unwrap(),
        1,
        "cold pending queue must hold 1 signal before ensure_loaded; got {:?}",
        cold_events
    );

    // Promote via the production dispatch path (acquires gate, runs ensure_loaded,
    // drains the pending queue).
    promote_namespace(&brain, &rt, "local").await;

    // After promotion: the pending queue must be empty (drained into active state).
    assert!(
        brain.cold_namespace_total_events("local").is_none(),
        "pending queue for 'local' must be empty after ensure_loaded drains it"
    );

    // The active snapshot must reflect the queued signal.
    let snap = brain.snapshot();
    assert_eq!(
        snap.balanced_recall.total_events, 1,
        "active snapshot total_events must be 1 after cold-hook signal survives promotion; got {}",
        snap.balanced_recall.total_events
    );
}

/// Two-namespace regression: signals routed to different namespaces must be
/// accounted independently, and neither must bleed into the other.
///
/// Registry A dispatches with namespace "ns-alpha"; registry B dispatches with
/// "ns-beta".  After 2 + 3 dispatches the cold buckets must hold 2 and 3
/// respectively.
///
/// Round-3 strengthening: after recording, both namespaces are promoted via
/// the production dispatch path and the totals must survive into each
/// respective active snapshot.
#[tokio::test]
async fn dispatch_hook_applies_signals_per_namespace_independently() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let brain = Arc::new(BrainPack::new(rt.clone()));
    // No prior activation — both namespaces are cold.

    let build_registry = |ns: &str| {
        let rt2 = rt.clone();
        let brain2 = brain.clone();
        let ns_owned = ns.to_string();
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt2));
        builder.with_default_namespace(ns_owned);
        let hook: Arc<dyn DispatchHook> = brain2;
        builder.with_dispatch_hook(hook);
        builder.build().expect("registry builds")
    };

    let reg_alpha = build_registry("ns-alpha");
    let reg_beta = build_registry("ns-beta");

    // 2 dispatches to ns-alpha.
    for i in 0..2u32 {
        reg_alpha
            .dispatch(
                "create",
                json!({"kind":"entity","name":format!("AlphaE{i}"),"entity_kind":"concept"}),
            )
            .await
            .expect("alpha dispatch");
    }
    // 3 dispatches to ns-beta.
    for i in 0..3u32 {
        reg_beta
            .dispatch(
                "create",
                json!({"kind":"entity","name":format!("BetaE{i}"),"entity_kind":"concept"}),
            )
            .await
            .expect("beta dispatch");
    }

    // Before promotion: pending queues must hold the correct counts.
    assert_eq!(
        brain.cold_namespace_total_events("ns-alpha"),
        Some(2),
        "ns-alpha pending queue must have exactly 2 signals before promotion"
    );
    assert_eq!(
        brain.cold_namespace_total_events("ns-beta"),
        Some(3),
        "ns-beta pending queue must have exactly 3 signals before promotion"
    );

    // Promote ns-alpha via the production dispatch path.
    promote_namespace(&brain, &rt, "ns-alpha").await;

    let snap_alpha = brain.snapshot();
    assert_eq!(
        snap_alpha.balanced_recall.total_events, 2,
        "ns-alpha active snapshot must show 2 events after promotion; got {}",
        snap_alpha.balanced_recall.total_events
    );
    assert!(
        brain.cold_namespace_total_events("ns-alpha").is_none(),
        "ns-alpha pending queue must be empty after promotion"
    );

    // ns-beta queue must still be intact while ns-alpha is active.
    assert_eq!(
        brain.cold_namespace_total_events("ns-beta"),
        Some(3),
        "ns-beta pending queue must remain 3 while ns-alpha is active"
    );

    // Promote ns-beta via the production dispatch path.
    promote_namespace(&brain, &rt, "ns-beta").await;

    let snap_beta = brain.snapshot();
    assert_eq!(
        snap_beta.balanced_recall.total_events, 3,
        "ns-beta active snapshot must show 3 events after promotion; got {}",
        snap_beta.balanced_recall.total_events
    );
    assert!(
        brain.cold_namespace_total_events("ns-beta").is_none(),
        "ns-beta pending queue must be empty after promotion"
    );
}

#[tokio::test]
async fn brain_pack_hook_does_not_fire_on_unknown_verb() {
    // Sanity: dispatch failure must not corrupt brain state. The hook only
    // fires on SUCCESSFUL dispatch — unknown verbs return an error before
    // the hook is invoked.
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let brain = Arc::new(BrainPack::new(rt.clone()));

    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt));
    let hook: Arc<dyn DispatchHook> = brain.clone();
    builder.with_dispatch_hook(hook);
    let registry = builder.build().expect("registry builds");

    let _ = registry.dispatch("frobnicate_nonexistent", json!({})).await;

    // The verb errored, so the cold bucket for "local" must not have been created.
    assert!(
        brain.cold_namespace_total_events("local").is_none(),
        "failed dispatch must NOT initialise the cold namespace pending queue"
    );
}

/// Snapshot + cold hook signal regression: if a namespace has persisted history
/// from prior `brain.feedback` calls, `ensure_loaded` must restore from the
/// persisted snapshot AND then apply any queued hook signals on top.
///
/// Sequence:
///   1. Create a real entity (needed for brain.feedback's C4 target validation).
///   2. Build a full registry (kg + brain) on the brain instance under test and
///      dispatch 5 `brain.feedback` calls — enough to cross the snapshot batch
///      threshold (DEFAULT_SNAPSHOT_BATCH_SIZE = 5) and force `upsert_snapshot`.
///      Record the persisted total (= 5).
///   3. Displace "local" from the active slot by loading a second namespace.
///   4. While "local" is saved off (is_loaded=true, not active), fire one KG
///      hook signal — it routes via the saved-state path in `route_signal`.
///   5. Reload "local" via `brain.dispatch("brain.profiles", …)`.
///   6. Assert active snapshot total == persisted_total + 1 queued signal.
#[tokio::test]
async fn cold_hook_signal_applies_on_top_of_persisted_snapshot() {
    use khive_runtime::Namespace;

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let brain = Arc::new(BrainPack::new(rt.clone()));

    // --- Step 1: create a real entity for feedback target validation ---
    // We need both kg and brain packs to (a) create entities and (b) dispatch
    // brain.feedback on the SAME brain instance we are testing.
    let full_registry = {
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(BrainPack::new(rt.clone()));
        builder.build().expect("full registry for step 1")
    };

    // Create an entity through the full registry to get a valid target_id UUID.
    let entity_result = full_registry
        .dispatch(
            "create",
            json!({
                "kind": "entity",
                "name": "SnapshotProbeTarget",
                "entity_kind": "concept"
            }),
        )
        .await
        .expect("create entity for feedback target");
    let target_id = entity_result["id"]
        .as_str()
        .expect("created entity must have id")
        .to_string();

    // --- Step 2: dispatch 5 brain.feedback calls to cross the snapshot threshold ---
    // DEFAULT_SNAPSHOT_BATCH_SIZE = 5; the 5th feedback call triggers upsert_snapshot.
    // We use brain.dispatch directly so the feedback accumulates on the brain
    // instance under test (not the full_registry's separate BrainPack instance).
    let empty_registry = VerbRegistryBuilder::new()
        .build()
        .expect("minimal registry");
    let local_ns = Namespace::parse("local").expect("local namespace");
    let local_token = rt.authorize(local_ns).expect("local token");

    // First promote the namespace so feedback does not start from a cold state.
    brain
        .dispatch("brain.profiles", json!({}), &empty_registry, &local_token)
        .await
        .expect("promote local namespace before feedback");

    for _ in 0..5u32 {
        brain
            .dispatch(
                "brain.feedback",
                json!({ "target_id": target_id, "signal": "useful" }),
                &empty_registry,
                &local_token,
            )
            .await
            .expect("brain.feedback must succeed");
    }

    // Record the persisted total before displacing the namespace.
    let persisted_total = brain.snapshot().balanced_recall.total_events;
    assert_eq!(
        persisted_total, 5,
        "after 5 feedback calls the active snapshot must show 5 total_events; got {persisted_total}"
    );

    // --- Step 3: displace "local" by loading a different namespace ---
    promote_namespace(&brain, &rt, "ns-other").await;

    // "local" is now in saved_states (is_loaded=true, not active).

    // --- Step 4: fire a KG hook signal while "local" is saved off ---
    // The hook routes via the saved-state path in route_signal (not the
    // pending queue, because is_loaded=true for "local").
    let mut hook_builder = VerbRegistryBuilder::new();
    hook_builder.register(KgPack::new(rt.clone()));
    hook_builder.with_default_namespace("local".to_string());
    let hook_arc: Arc<dyn DispatchHook> = brain.clone();
    hook_builder.with_dispatch_hook(hook_arc);
    let hook_registry = hook_builder.build().expect("hook registry");

    hook_registry
        .dispatch(
            "create",
            json!({"kind":"entity","name":"SavedPathProbe","entity_kind":"concept"}),
        )
        .await
        .expect("kg dispatch must succeed");

    // --- Step 5: reload "local" via the production dispatch path ---
    brain
        .dispatch("brain.profiles", json!({}), &empty_registry, &local_token)
        .await
        .expect("reload local via production path");

    // --- Step 6: assert total = persisted (5) + saved-state signal (1) ---
    let snap = brain.snapshot();
    assert_eq!(
        snap.balanced_recall.total_events,
        persisted_total + 1,
        "active snapshot for 'local' must equal persisted total ({persisted_total}) + \
         1 saved-state signal; got {}",
        snap.balanced_recall.total_events
    );
}

// ---- Finding 3 regression: brain.feedback target_id must be primary-only ----

/// `brain.feedback` must reject a `target_id` that lives in a visible (non-primary)
/// namespace. A visible-only record must not be the target of a feedback mutation.
///
/// This tests that the fix from `resolve` to `resolve_primary` is in effect:
/// a record the caller can READ but not MUTATE returns NotFound on feedback.
#[tokio::test]
async fn brain_feedback_rejects_visible_only_target_id() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");

    let ns_primary = Namespace::parse("brain-primary-ns").unwrap();
    let ns_foreign = Namespace::parse("brain-foreign-ns").unwrap();

    // Create a KG entity in the foreign namespace.
    let tok_foreign = rt.authorize(ns_foreign.clone()).unwrap();
    let foreign_entity = rt
        .create_entity(
            &tok_foreign,
            "concept",
            None,
            "ForeignTarget",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
    let foreign_id = foreign_entity.id.as_hyphenated().to_string();

    // Build a visible-set token: primary-ns can read (but not mutate) foreign-ns.
    let tok_vis = rt
        .authorize_with_visibility(ns_primary.clone(), vec![ns_foreign.clone()])
        .unwrap();

    // Verify the visible-set token CAN read the foreign entity (control check).
    let found = rt.get_entity(&tok_vis, foreign_entity.id).await;
    assert!(
        found.is_ok(),
        "visible-set token must be able to read the foreign entity; got: {found:?}"
    );

    // brain.feedback with the foreign entity as target_id must fail NotFound.
    let brain = BrainPack::new(rt.clone());
    let empty_registry = VerbRegistryBuilder::new().build().unwrap();

    // First promote the primary namespace.
    brain
        .dispatch("brain.profiles", json!({}), &empty_registry, &tok_vis)
        .await
        .expect("promote primary namespace");

    let err = brain
        .dispatch(
            "brain.feedback",
            json!({ "target_id": foreign_id, "signal": "useful" }),
            &empty_registry,
            &tok_vis,
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.to_lowercase().contains("not found"),
        "brain.feedback with visible-only target_id must return NotFound; got: {msg}"
    );
}
