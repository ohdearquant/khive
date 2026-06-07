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

#[tokio::test]
async fn brain_pack_dispatch_hook_records_real_dispatch_events() {
    // Build a real runtime + brain pack. Wrap the brain in Arc so we can both
    // register it as a hook AND hold a reference to read its state afterward.
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let brain = Arc::new(BrainPack::new(rt.clone()));

    let baseline = brain.snapshot();

    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt));
    let hook: Arc<dyn DispatchHook> = brain.clone();
    builder.with_dispatch_hook(hook);
    let registry = builder.build().expect("registry builds");

    // Fire a real verb. KG `create` is a normal dispatch — the hook must
    // observe it via on_dispatch.
    let _ = registry
        .dispatch(
            "create",
            json!({
                "kind": "entity",
                "name": "HookProbe",
                "entity_kind": "concept"
            }),
        )
        .await
        .expect("create entity must succeed");

    // Every successful dispatch increments BalancedRecallState.total_events via
    // BalancedRecallFold::reduce. If the hook never fired, the counter stays at
    // baseline.
    let after = brain.snapshot();
    assert_eq!(
        after.balanced_recall.total_events,
        baseline.balanced_recall.total_events + 1,
        "#158 regression: total_events did not advance after a successful KG \
         verb dispatch. baseline={}, after={}",
        baseline.balanced_recall.total_events,
        after.balanced_recall.total_events,
    );

    // Fire two more successful dispatches and verify the counter advances by
    // exactly N — proves the hook fires per-dispatch, not once-per-session.
    for i in 0..2 {
        let _ = registry
            .dispatch(
                "create",
                json!({
                    "kind": "entity",
                    "name": format!("HookProbe{i}"),
                    "entity_kind": "concept"
                }),
            )
            .await
            .expect("subsequent create must succeed");
    }
    let final_state = brain.snapshot();
    assert_eq!(
        final_state.balanced_recall.total_events,
        baseline.balanced_recall.total_events + 3,
        "hook must fire once per successful dispatch: expected {}+3 events, got {}",
        baseline.balanced_recall.total_events,
        final_state.balanced_recall.total_events,
    );
}

#[tokio::test]
async fn brain_pack_hook_does_not_fire_on_unknown_verb() {
    // Sanity: dispatch failure must not corrupt brain state. The hook only
    // fires on SUCCESSFUL dispatch — unknown verbs return an error before
    // the hook is invoked.
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let brain = Arc::new(BrainPack::new(rt.clone()));
    let baseline = brain.snapshot();

    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt));
    let hook: Arc<dyn DispatchHook> = brain.clone();
    builder.with_dispatch_hook(hook);
    let registry = builder.build().expect("registry builds");

    let _ = registry.dispatch("frobnicate_nonexistent", json!({})).await;

    let after = brain.snapshot();
    // The verb errored, so BalancedRecallState.total_events must be unchanged.
    assert_eq!(
        after.balanced_recall.total_events, baseline.balanced_recall.total_events,
        "unknown verb must NOT change brain state — got {}, baseline had {}",
        after.balanced_recall.total_events, baseline.balanced_recall.total_events,
    );
    // The profile registry is also unchanged
    assert_eq!(
        after.profiles.len(),
        baseline.profiles.len(),
        "profile registry must not change on failed dispatch"
    );
}
