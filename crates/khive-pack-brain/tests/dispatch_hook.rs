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

    // The signal must have been applied to the cold state bucket for "local".
    let cold_events = brain.cold_namespace_total_events("local");
    assert!(
        cold_events.is_some(),
        "cold-namespace 'local' state must have been initialised by the hook"
    );
    assert_eq!(
        cold_events.unwrap(),
        1,
        "cold-namespace total_events must be 1 after one dispatch; got {:?}",
        cold_events
    );
}

/// Two-namespace regression: signals routed to different namespaces must be
/// accounted independently, and neither must bleed into the other.
///
/// Registry A dispatches with namespace "ns-alpha"; registry B dispatches with
/// "ns-beta".  After 2 + 3 dispatches the cold buckets must hold 2 and 3
/// respectively.
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

    assert_eq!(
        brain.cold_namespace_total_events("ns-alpha"),
        Some(2),
        "ns-alpha must have exactly 2 events"
    );
    assert_eq!(
        brain.cold_namespace_total_events("ns-beta"),
        Some(3),
        "ns-beta must have exactly 3 events"
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
        "failed dispatch must NOT initialise the cold namespace bucket"
    );
}
