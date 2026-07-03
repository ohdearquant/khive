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

    // Promote via the production dispatch path (acquires gate, runs ensure_loaded,
    // drains the pending queue).
    promote_namespace(&brain, &rt, "local").await;

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

    // Promote ns-alpha via the production dispatch path.
    promote_namespace(&brain, &rt, "ns-alpha").await;

    let snap_alpha = brain.snapshot();
    assert_eq!(
        snap_alpha.balanced_recall.total_events, 2,
        "ns-alpha active snapshot must show 2 events after promotion; got {}",
        snap_alpha.balanced_recall.total_events
    );

    // Promote ns-beta via the production dispatch path.
    promote_namespace(&brain, &rt, "ns-beta").await;

    let snap_beta = brain.snapshot();
    assert_eq!(
        snap_beta.balanced_recall.total_events, 3,
        "ns-beta active snapshot must show 3 events after promotion; got {}",
        snap_beta.balanced_recall.total_events
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
    builder.register(KgPack::new(rt.clone()));
    let hook: Arc<dyn DispatchHook> = brain.clone();
    builder.with_dispatch_hook(hook);
    let registry = builder.build().expect("registry builds");

    let _ = registry.dispatch("frobnicate_nonexistent", json!({})).await;

    // Promote "local" via the production path to flush any pending signals.
    // A failed dispatch must not have enqueued any signal, so total_events stays 0.
    promote_namespace(&brain, &rt, "local").await;

    let snap = brain.snapshot();
    assert_eq!(
        snap.balanced_recall.total_events, 0,
        "failed dispatch must NOT fire the hook; total_events must remain 0 after promotion; got {}",
        snap.balanced_recall.total_events
    );
}

/// Snapshot + cold hook signal regression: if a namespace has persisted history
/// from prior `brain.feedback` calls, `ensure_loaded` must restore from the
/// persisted snapshot AND then apply any queued hook signals on top.
///
/// This test exercises the TRUE DB cold path: a second `BrainPack` instance is
/// constructed over the same `KhiveRuntime` (same SQLite DB) with no in-memory
/// state.  The queued signal is enqueued while the namespace is genuinely unknown
/// to the second instance (goes to `pending_hook_signals`, not `saved_states`).
/// On first access the second instance must reload from the persisted DB snapshot
/// and drain the pending queue on top.
///
/// Sequence:
///   1. Create a real entity for brain.feedback target validation.
///   2. Using brain_a, dispatch 5 brain.feedback calls — enough to cross
///      DEFAULT_SNAPSHOT_BATCH_SIZE (5) and force upsert_snapshot into the
///      shared DB.  Record persisted_total == 5.
///   3. Construct brain_b = BrainPack::new(rt.clone()), a fresh instance over
///      the same runtime/DB with empty in-memory state.  brain_a is no longer
///      used; its in-memory state is not accessible to brain_b.
///   4. Fire one KG hook signal through brain_b for the "local" namespace.
///      Because brain_b has never loaded "local", route_signal enqueues it in
///      pending_hook_signals (true cold pending path).
///   5. Promote "local" in brain_b via the production dispatch path
///      (brain_b.dispatch("brain.profiles", …)).  ensure_loaded must:
///        a. load the DB snapshot (5 events)
///        b. drain the 1 pending hook signal on top
///        → total_events = 6
///   6. Assert brain_b.snapshot().balanced_recall.total_events == persisted_total + 1.
#[tokio::test]
async fn cold_hook_signal_applies_on_top_of_persisted_snapshot() {
    use khive_runtime::Namespace;

    let rt = KhiveRuntime::memory().expect("in-memory runtime");

    // --- Step 1: create a real entity for feedback target validation ---
    // A separate brain/kg registry creates the entity; the entity UUID is used
    // by brain_a for brain.feedback target validation.
    let setup_registry = {
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(BrainPack::new(rt.clone()));
        builder.build().expect("setup registry")
    };
    let entity_result = setup_registry
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

    // --- Step 2: dispatch 5 brain.feedback calls via brain_a ---
    // DEFAULT_SNAPSHOT_BATCH_SIZE = 5; the 5th call triggers upsert_snapshot,
    // writing the "local" namespace snapshot into the shared DB.
    let brain_a = BrainPack::new(rt.clone());
    let empty_registry = VerbRegistryBuilder::new()
        .build()
        .expect("minimal registry");
    let local_ns = Namespace::parse("local").expect("local namespace");
    let local_token = rt.authorize(local_ns).expect("local token");

    brain_a
        .dispatch("brain.profiles", json!({}), &empty_registry, &local_token)
        .await
        .expect("promote local namespace in brain_a before feedback");

    for _ in 0..5u32 {
        brain_a
            .dispatch(
                "brain.feedback",
                json!({ "target_id": target_id, "signal": "useful" }),
                &empty_registry,
                &local_token,
            )
            .await
            .expect("brain.feedback must succeed");
    }

    let persisted_total = brain_a.snapshot().balanced_recall.total_events;
    assert_eq!(
        persisted_total, 5,
        "after 5 feedback calls brain_a snapshot must show 5 total_events; got {persisted_total}"
    );

    // --- Step 3: construct brain_b — fresh instance, empty in-memory state ---
    // brain_a is dropped after this point; brain_b shares the same SQLite DB
    // via rt.clone() but starts with a completely empty PersistenceTracker and
    // BrainState.  This is the true DB cold-start scenario.
    drop(brain_a);
    let brain_b = Arc::new(BrainPack::new(rt.clone()));

    // --- Step 4: fire one KG hook signal through brain_b ---
    // brain_b has never seen "local"; route_signal must enqueue the signal in
    // pending_hook_signals (cold pending path, not the saved_states path).
    let mut hook_builder = VerbRegistryBuilder::new();
    hook_builder.register(KgPack::new(rt.clone()));
    hook_builder.with_default_namespace("local".to_string());
    let hook_arc: Arc<dyn DispatchHook> = brain_b.clone();
    hook_builder.with_dispatch_hook(hook_arc);
    let hook_registry = hook_builder.build().expect("hook registry for brain_b");

    hook_registry
        .dispatch(
            "create",
            json!({"kind":"entity","name":"ColdReplayProbe","entity_kind":"concept"}),
        )
        .await
        .expect("kg dispatch through brain_b hook must succeed");

    // --- Step 5: promote "local" in brain_b via the production dispatch path ---
    // ensure_loaded must:
    //   a. load the persisted DB snapshot (total_events = 5 from brain_a's writes)
    //   b. drain the 1 pending hook signal enqueued in step 4
    // If either sub-step is broken the final count will not equal 6.
    brain_b
        .dispatch("brain.profiles", json!({}), &empty_registry, &local_token)
        .await
        .expect("promote local namespace in brain_b via cold DB load");

    // --- Step 6: assert total = persisted (5) + cold pending signal (1) ---
    let snap = brain_b.snapshot();
    assert_eq!(
        snap.balanced_recall.total_events,
        persisted_total + 1,
        "brain_b cold-reload must yield persisted total ({persisted_total}) + \
         1 queued signal; got {}",
        snap.balanced_recall.total_events
    );
}

// ---- bfbb65d1 regression: brain.feedback target_id is namespace-agnostic ----

/// `brain.feedback` must ACCEPT a `target_id` that lives in another namespace.
///
/// ADR-007 Rule 2 / PR-A1: by-ID ops (get / update / delete / merge — and
/// feedback) resolve a globally-unique UUID with no namespace check; the Gate
/// owns authorization, not a post-fetch namespace comparison. Rule 3b recall
/// fans out actor-stamped memories from other namespaces by design (a lambda's
/// episodic memories carry its actor namespace), so a primary-only check
/// rejected the flywheel's own recalled targets — exactly the fleet-wide
/// feedback-discipline breakage this fixes.
///
/// This reverses the earlier "Finding 3" primary-only behavior, which cited
/// ADR-007 for a rule the document does not contain (the word "feedback" never
/// appears in ADR-007; lines 213-221 are the dispatch-boundary token-minting
/// paragraph, unrelated to targeting foreign records).
#[tokio::test]
async fn brain_feedback_accepts_foreign_namespace_target_id() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");

    let ns_primary = Namespace::parse("brain-primary-ns").unwrap();
    let ns_foreign = Namespace::parse("brain-foreign-ns").unwrap();

    // A recalled note (the episodic-memory analog) stamped in the foreign
    // namespace — the Rule 3b fanout target class.
    let tok_foreign = rt.authorize(ns_foreign.clone()).unwrap();
    let foreign_note = rt
        .create_note(
            &tok_foreign,
            "observation",
            None,
            "foreign-namespace recalled memory",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
    let foreign_id = foreign_note.id.as_hyphenated().to_string();

    // Caller in a different primary namespace with NO visibility grant for the
    // foreign ns. resolve_by_id is ID-only (ignores namespace AND visible set),
    // so feedback must still resolve the foreign target. Using a bare token pins
    // the FULL ns-agnostic contract: this test fails under the old primary-only
    // `resolve_primary` AND under any future visible-set-gated `resolve`.
    let tok_primary = rt.authorize(ns_primary.clone()).unwrap();

    let brain = BrainPack::new(rt.clone());
    let empty_registry = VerbRegistryBuilder::new().build().unwrap();
    brain
        .dispatch("brain.profiles", json!({}), &empty_registry, &tok_primary)
        .await
        .expect("promote primary namespace");

    // Feedback on the foreign-namespace target now RESOLVES and emits
    // (previously returned NotFound under the primary-only check).
    let out = brain
        .dispatch(
            "brain.feedback",
            json!({ "target_id": foreign_id, "signal": "useful" }),
            &empty_registry,
            &tok_primary,
        )
        .await;
    assert!(
        out.is_ok(),
        "brain.feedback on a foreign-namespace target must succeed \
         (ADR-007 Rule 2 by-ID resolution); got: {out:?}"
    );

    // A genuinely absent UUID still returns NotFound.
    let absent = "00000000-0000-4000-8000-000000000000";
    let err = brain
        .dispatch(
            "brain.feedback",
            json!({ "target_id": absent, "signal": "useful" }),
            &empty_registry,
            &tok_primary,
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("not found"),
        "brain.feedback on an absent target_id must return NotFound; got: {err}"
    );
}
