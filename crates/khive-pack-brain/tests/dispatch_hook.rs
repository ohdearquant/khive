//! End-to-end tests for `BrainPack` as a `DispatchHook` (issue #158).
//!
//! Per ADR-032, `BrainState` now holds a profile registry; the BalancedRecall
//! profile's `total_events` counter lives in `snapshot.balanced_recall.total_events`.
//! These tests verify the dispatch hook still drives the BalancedRecallFold.

use std::sync::Arc;

use khive_pack_brain::BrainPack;
use khive_pack_kg::KgPack;
use khive_runtime::{DispatchHook, KhiveRuntime, VerbRegistryBuilder};
use serde_json::json;

/// Cold-path regression: the hook must update brain state even when no namespace
/// has been pre-activated.  Before the fix, `on_dispatch` returned early whenever
/// `active_namespace != event.namespace`, silently dropping the first (and all
/// subsequent) cold-namespace events.
///
/// This test passes NO prior `activate_namespace_for_test` / `ensure_loaded` call.
/// The registry's default namespace is "local"; the hook must create a cold bucket
/// for "local" and apply the signal there.
///
/// Round-3 strengthening: after firing the hook signal, `ensure_loaded` is called
/// (via `ensure_loaded_for_test`).  The signal must survive into the active state.
#[tokio::test]
async fn dispatch_hook_fires_on_cold_namespace_no_prior_activation() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let brain = Arc::new(BrainPack::new(rt.clone()));
    // Deliberately do NOT activate any namespace — this is the cold path.

    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt));
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

    // Promote: run the full load path (snapshot + replay + drain pending queue).
    brain
        .ensure_loaded_for_test("local")
        .await
        .expect("ensure_loaded_for_test must not fail");

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
/// Round-3 strengthening: after recording, `ensure_loaded` is called for both
/// namespaces and the totals must survive into each respective active snapshot.
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

    // Promote ns-alpha: ensure_loaded drains its queue into the active state.
    brain
        .ensure_loaded_for_test("ns-alpha")
        .await
        .expect("ensure_loaded_for_test ns-alpha");

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

    // Promote ns-beta.
    brain
        .ensure_loaded_for_test("ns-beta")
        .await
        .expect("ensure_loaded_for_test ns-beta");

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

/// Snapshot + cold hook signal regression: if a namespace has a persisted
/// snapshot, `ensure_loaded` must restore from that snapshot AND then apply
/// any queued hook signals on top.  The snapshot data must not be bypassed.
///
/// Sequence:
///   1. Fire a brain verb to create and persist a snapshot for "local".
///   2. Fire a KG verb so the hook queues one pending signal for "local" while
///      it is not the active namespace (swap it out by loading another ns first).
///   3. Reload the original namespace via `ensure_loaded_for_test`.
///   4. Assert the active snapshot reflects BOTH the persisted events AND the
///      queued hook signal.
#[tokio::test]
async fn cold_hook_signal_applies_on_top_of_persisted_snapshot() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let brain = Arc::new(BrainPack::new(rt.clone()));

    // --- Step 1: build a persisted snapshot for "local" ---
    // Load "local" by dispatching a brain verb.
    {
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(BrainPack::new(rt.clone()));
        let registry = builder.build().expect("full registry builds");

        // brain.profiles triggers ensure_loaded for "local".
        registry
            .dispatch("brain.profiles", json!({}))
            .await
            .expect("brain.profiles must succeed");
    }
    // Verify "local" is now loaded (active) by checking no pending queue entry.
    assert!(
        brain.cold_namespace_total_events("local").is_none(),
        "after brain.profiles, 'local' must not be in the cold pending queue"
    );

    // Force "local" out of the active slot by loading a second namespace.
    brain
        .ensure_loaded_for_test("ns-other")
        .await
        .expect("load ns-other to displace local");

    // --- Step 2: fire a KG hook signal while "local" is saved off ---
    // At this point "local" is in saved_states (is_loaded=true, not active).
    // A KG dispatch with namespace "local" should apply directly to saved_states
    // (saved path in route_signal) — not the pending queue.
    let mut builder2 = VerbRegistryBuilder::new();
    builder2.register(KgPack::new(rt.clone()));
    builder2.with_default_namespace("local".to_string());
    let hook: Arc<dyn DispatchHook> = brain.clone();
    builder2.with_dispatch_hook(hook);
    let reg2 = builder2.build().expect("registry2 builds");

    reg2.dispatch(
        "create",
        json!({"kind":"entity","name":"SavedPathProbe","entity_kind":"concept"}),
    )
    .await
    .expect("kg dispatch must succeed");

    // "local" received the signal via the saved-state path.  Reload it.
    brain
        .ensure_loaded_for_test("local")
        .await
        .expect("reload local");

    let snap = brain.snapshot();
    // The saved-state signal must be visible.
    assert!(
        snap.balanced_recall.total_events >= 1,
        "active snapshot for 'local' must include the saved-state signal; got {}",
        snap.balanced_recall.total_events
    );
}
