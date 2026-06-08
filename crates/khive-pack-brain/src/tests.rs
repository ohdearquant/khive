//! Integration tests for `BrainPack` dispatch.
use super::*;
use khive_runtime::{
    DispatchHook, KhiveRuntime, Namespace, NamespaceToken, PackRuntime, RuntimeError,
    VerbRegistryBuilder,
};
use khive_types::HandlerDef;
use serde_json::json;

fn make_pack() -> (BrainPack, KhiveRuntime) {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let pack = BrainPack::new(rt.clone());
    (pack, rt)
}

fn empty_registry() -> khive_runtime::VerbRegistry {
    VerbRegistryBuilder::new()
        .build()
        .expect("empty registry builds successfully")
}

/// Create a real entity in the runtime and return its UUID string.
/// Used by feedback tests that need a valid target_id (C4 validation).
async fn create_test_entity(rt: &KhiveRuntime, token: &NamespaceToken) -> String {
    let entity = rt
        .create_entity(token, "concept", None, "test-target", None, None, vec![])
        .await
        .expect("create test entity");
    entity.id.to_string()
}

#[tokio::test]
async fn dispatch_unknown_verb_returns_invalid_input() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let err = pack
        .dispatch(
            "brain.unknown",
            json!({}),
            &registry,
            &rt.authorize(Namespace::local()).unwrap(),
        )
        .await
        .unwrap_err();
    if let RuntimeError::InvalidInput(msg) = &err {
        assert!(
            msg.contains("brain.unknown"),
            "expected verb name in error: {msg}"
        );
    } else {
        panic!("expected InvalidInput, got {err:?}");
    }
}

#[tokio::test]
async fn dispatch_reset_returns_true_and_increments_epoch() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let result = pack
        .dispatch(
            "brain.reset",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &rt.authorize(Namespace::local()).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(result["reset"], json!(true));
    assert_eq!(result["exploration_epoch"], json!(1u64));
    assert_eq!(result["profile_id"], json!("balanced-recall-v1"));
}

#[tokio::test]
async fn dispatch_reset_no_args_resets_default_profile() {
    // profile_id is optional; omitting it resets balanced-recall-v1 by default.
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let result = pack
        .dispatch(
            "brain.reset",
            json!({}),
            &registry,
            &rt.authorize(Namespace::local()).unwrap(),
        )
        .await
        .expect("reset with no args must succeed (defaults to balanced-recall-v1)");
    assert_eq!(result["reset"], json!(true));
    assert_eq!(result["profile_id"], json!("balanced-recall-v1"));
}

#[tokio::test]
async fn dispatch_reset_nonexistent_profile_returns_not_found() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let err = pack
        .dispatch(
            "brain.reset",
            json!({"profile_id": "ghost-profile"}),
            &registry,
            &rt.authorize(Namespace::local()).unwrap(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::NotFound(_)),
        "reset on nonexistent profile must return NotFound, got {err:?}"
    );
}

#[tokio::test]
async fn dispatch_reset_archived_profile_returns_invalid_input() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    // Archive the profile via the lifecycle DAG
    pack.dispatch(
        "brain.deactivate",
        json!({"profile_id": "balanced-recall-v1"}),
        &registry,
        &token,
    )
    .await
    .unwrap();
    pack.dispatch(
        "brain.archive",
        json!({"profile_id": "balanced-recall-v1"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    let err = pack
        .dispatch(
            "brain.reset",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    if let RuntimeError::InvalidInput(msg) = &err {
        assert!(
            msg.contains("archived") || msg.contains("terminal"),
            "reset on archived profile must mention 'archived' or 'terminal'; got: {msg}"
        );
    } else {
        panic!("reset on archived profile must return InvalidInput, got {err:?}");
    }
}

#[tokio::test]
async fn dispatch_feedback_invalid_signal_returns_invalid_input() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let target = "00000000-0000-0000-0000-000000000001";
    let err = pack
        .dispatch(
            "brain.feedback",
            json!({"target_id": target, "signal": "bad_signal"}),
            &registry,
            &rt.authorize(Namespace::local()).unwrap(),
        )
        .await
        .unwrap_err();
    if let RuntimeError::InvalidInput(msg) = &err {
        assert!(
            msg.contains("bad_signal"),
            "expected signal name in error: {msg}"
        );
        assert!(
            msg.contains("valid"),
            "expected hint about valid values: {msg}"
        );
    } else {
        panic!("expected InvalidInput, got {err:?}");
    }
}

#[tokio::test]
async fn dispatch_state_returns_snapshot_fields() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let result = pack
        .dispatch(
            "brain.state",
            json!({}),
            &registry,
            &rt.authorize(Namespace::local()).unwrap(),
        )
        .await
        .unwrap();
    assert!(result.get("profiles").is_some(), "missing profiles");
    assert!(
        result.get("balanced_recall").is_some(),
        "missing balanced_recall"
    );
    assert!(result.get("bindings").is_some(), "missing bindings");
}

#[tokio::test]
async fn dispatch_profiles_returns_default_profile() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let result = pack
        .dispatch(
            "brain.profiles",
            json!({}),
            &registry,
            &rt.authorize(Namespace::local()).unwrap(),
        )
        .await
        .unwrap();
    let profiles = result["profiles"].as_array().unwrap();
    assert!(!profiles.is_empty(), "expected at least one profile");
    assert_eq!(profiles[0]["id"], json!("balanced-recall-v1"));
}

#[tokio::test]
async fn dispatch_profiles_filtered_by_lifecycle() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let result = pack
        .dispatch(
            "brain.profiles",
            json!({"lifecycle": "active"}),
            &registry,
            &rt.authorize(Namespace::local()).unwrap(),
        )
        .await
        .unwrap();
    let profiles = result["profiles"].as_array().unwrap();
    for p in profiles {
        assert_eq!(p["lifecycle"], json!("active"));
    }
}

#[tokio::test]
async fn dispatch_profile_returns_profile_details() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let result = pack
        .dispatch(
            "brain.profile",
            json!({"id": "balanced-recall-v1"}),
            &registry,
            &rt.authorize(Namespace::local()).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(result["id"], json!("balanced-recall-v1"));
    assert_eq!(result["state_class"], json!("Bayesian"));
    assert_eq!(result["consumer_kind"], json!("recall"));
}

#[tokio::test]
async fn dispatch_profile_not_found_returns_not_found() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let err = pack
        .dispatch(
            "brain.profile",
            json!({"id": "nonexistent"}),
            &registry,
            &rt.authorize(Namespace::local()).unwrap(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, RuntimeError::NotFound(_)));
}

#[tokio::test]
async fn dispatch_resolve_returns_default_profile_for_recall() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let result = pack
        .dispatch(
            "brain.resolve",
            json!({"consumer_kind": "recall"}),
            &registry,
            &rt.authorize(Namespace::local()).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(result["resolved_profile_id"], json!("balanced-recall-v1"));
}

#[tokio::test]
async fn dispatch_activate_and_deactivate_profile() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    // Deactivate the default profile
    let result = pack
        .dispatch(
            "brain.deactivate",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(result["lifecycle"], json!("inactive"));

    // Verify via brain.profile
    let state = pack
        .dispatch(
            "brain.profile",
            json!({"id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(state["lifecycle"], json!("inactive"));

    // Reactivate
    let result = pack
        .dispatch(
            "brain.activate",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(result["lifecycle"], json!("active"));
}

#[tokio::test]
async fn dispatch_archive_profile() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    // Lifecycle DAG requires active → inactive before archiving.
    pack.dispatch(
        "brain.deactivate",
        json!({"profile_id": "balanced-recall-v1"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    let result = pack
        .dispatch(
            "brain.archive",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(result["lifecycle"], json!("archived"));
}

#[tokio::test]
async fn dispatch_activate_nonexistent_profile_returns_not_found() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let err = pack
        .dispatch(
            "brain.activate",
            json!({"profile_id": "ghost-profile"}),
            &registry,
            &rt.authorize(Namespace::local()).unwrap(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, RuntimeError::NotFound(_)));
}

#[tokio::test]
async fn dispatch_bind_and_resolve_explicit_binding() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    // Bind balanced-recall-v1 for actor "agent-x"
    let result = pack
        .dispatch(
            "brain.bind",
            json!({
                "profile_id": "balanced-recall-v1",
                "actor": "agent-x",
                "consumer_kind": "recall"
            }),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(result["bound"], json!(true));
    assert_eq!(result["actor"], json!("agent-x"));

    // Resolve — should return the explicitly bound profile
    let resolved = pack
        .dispatch(
            "brain.resolve",
            json!({"actor": "agent-x", "consumer_kind": "recall"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(resolved["resolved_profile_id"], json!("balanced-recall-v1"));
}

#[tokio::test]
async fn dispatch_bind_nonexistent_profile_returns_not_found() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let err = pack
        .dispatch(
            "brain.bind",
            json!({"profile_id": "ghost", "consumer_kind": "recall"}),
            &registry,
            &rt.authorize(Namespace::local()).unwrap(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, RuntimeError::NotFound(_)));
}

#[tokio::test]
async fn dispatch_unbind_removes_binding() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    // Add a binding
    pack.dispatch(
        "brain.bind",
        json!({"profile_id": "balanced-recall-v1", "actor": "agent-y", "consumer_kind": "recall"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    // Remove it
    let result = pack
        .dispatch(
            "brain.unbind",
            json!({"actor": "agent-y"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(result["unbound"], json!(1u64));
}

// ── UE5-H1: brain.profiles lifecycle filter public API ───────────────────
//
// The public lifecycle filter must only accept 'active', 'inactive', 'archived'.
// Internal states 'defined' and 'registered' must not appear in the error message.

#[tokio::test]
async fn ue5_h1_invalid_lifecycle_error_lists_only_public_states() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    let err = pack
        .dispatch(
            "brain.profiles",
            json!({"lifecycle": "deleted"}),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    if let RuntimeError::InvalidInput(msg) = &err {
        // Must mention valid states
        assert!(
            msg.contains("active"),
            "UE5-H1: error must list 'active'; got: {msg}"
        );
        assert!(
            msg.contains("inactive"),
            "UE5-H1: error must list 'inactive'; got: {msg}"
        );
        assert!(
            msg.contains("archived"),
            "UE5-H1: error must list 'archived'; got: {msg}"
        );
        // Must NOT leak internal states
        assert!(
            !msg.contains("defined"),
            "UE5-H1: error must NOT expose internal 'defined' state; got: {msg}"
        );
        assert!(
            !msg.contains("registered"),
            "UE5-H1: error must NOT expose internal 'registered' state; got: {msg}"
        );
    } else {
        panic!("UE5-H1: expected InvalidInput, got {err:?}");
    }
}

#[tokio::test]
async fn ue5_h1_internal_lifecycle_values_rejected() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    for internal_state in ["defined", "registered"] {
        let err = pack
            .dispatch(
                "brain.profiles",
                json!({"lifecycle": internal_state}),
                &registry,
                &token,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, RuntimeError::InvalidInput(_)),
            "UE5-H1: internal lifecycle '{internal_state}' must be rejected, got {err:?}"
        );
    }
}

// ── B-C1 regression: lifecycle terminal-state enforcement ─────────────────
//
// archived is terminal: no transition out of archived is permitted.
// active → archived is illegal: must deactivate first.
// active ⟷ inactive is the only reversible pair.

#[tokio::test]
async fn b_c1_archived_activate_is_rejected() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    // Deactivate first (active → inactive), then archive (inactive → archived)
    pack.dispatch(
        "brain.deactivate",
        json!({"profile_id": "balanced-recall-v1"}),
        &registry,
        &token,
    )
    .await
    .unwrap();
    pack.dispatch(
        "brain.archive",
        json!({"profile_id": "balanced-recall-v1"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    // Attempt to activate an archived profile — must fail
    let err = pack
        .dispatch(
            "brain.activate",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    if let RuntimeError::InvalidInput(msg) = &err {
        assert!(
            msg.contains("terminal") || msg.contains("archived"),
            "B-C1: error must mention 'terminal' or 'archived'; got: {msg}"
        );
    } else {
        panic!("B-C1: expected InvalidInput, got {err:?}");
    }
}

#[tokio::test]
async fn b_c1_archived_deactivate_is_rejected() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    // Get to archived via active → inactive → archived
    pack.dispatch(
        "brain.deactivate",
        json!({"profile_id": "balanced-recall-v1"}),
        &registry,
        &token,
    )
    .await
    .unwrap();
    pack.dispatch(
        "brain.archive",
        json!({"profile_id": "balanced-recall-v1"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    // Attempt deactivate on archived profile — must fail
    let err = pack
        .dispatch(
            "brain.deactivate",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "B-C1: deactivate on archived must return InvalidInput, got {err:?}"
    );
}

#[tokio::test]
async fn b_c1_active_to_archived_direct_is_rejected() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    // Profile starts active — direct archive must fail (must go through inactive)
    let err = pack
        .dispatch(
            "brain.archive",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    if let RuntimeError::InvalidInput(msg) = &err {
        assert!(
            msg.contains("deactivate") || msg.contains("inactive"),
            "B-C1: active→archived error must hint at deactivate; got: {msg}"
        );
    } else {
        panic!("B-C1: expected InvalidInput for active→archived, got {err:?}");
    }
}

#[tokio::test]
async fn b_c1_inactive_to_archived_is_permitted() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    // active → inactive → archived: legal path
    pack.dispatch(
        "brain.deactivate",
        json!({"profile_id": "balanced-recall-v1"}),
        &registry,
        &token,
    )
    .await
    .unwrap();
    let result = pack
        .dispatch(
            "brain.archive",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(
        result["lifecycle"],
        json!("archived"),
        "B-C1: inactive→archived must succeed"
    );
}

// Regression test for MAJ-002: unbind with multiple filters must use AND semantics,
// removing only the binding that satisfies ALL supplied criteria.
#[tokio::test]
async fn dispatch_unbind_uses_and_not_or() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    // binding 1: ns=A, profile=P1 (the one we want to remove)
    pack.dispatch(
        "brain.bind",
        json!({"profile_id": "balanced-recall-v1", "namespace": "ns-a", "consumer_kind": "recall"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    // binding 2: ns=B, profile=P1 (must survive)
    pack.dispatch(
        "brain.bind",
        json!({"profile_id": "balanced-recall-v1", "namespace": "ns-b", "consumer_kind": "recall"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    // Unbind using both filters: only binding-1 should be removed
    let result = pack
        .dispatch(
            "brain.unbind",
            json!({"namespace": "ns-a", "profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(
        result["unbound"],
        json!(1u64),
        "should remove exactly one binding"
    );

    // binding-2 (ns-b) must still exist
    let state = pack.state.lock().unwrap();
    let remaining: Vec<_> = state
        .bindings
        .iter()
        .filter(|b| b.namespace == "ns-b")
        .collect();
    assert_eq!(remaining.len(), 1, "ns-b binding must survive the unbind");
}

#[tokio::test]
async fn dispatch_config_all_parameters() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let result = pack
        .dispatch(
            "brain.config",
            json!({}),
            &registry,
            &rt.authorize(Namespace::local()).unwrap(),
        )
        .await
        .unwrap();
    let obj = result.as_object().unwrap();
    assert!(obj.contains_key("recall::relevance_weight"));
    assert!(obj.contains_key("recall::salience_weight"));
    assert!(obj.contains_key("recall::temporal_weight"));
}

#[tokio::test]
async fn dispatch_config_single_parameter() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let result = pack
        .dispatch(
            "brain.config",
            json!({"parameter": "recall::relevance_weight"}),
            &registry,
            &rt.authorize(Namespace::local()).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(result["parameter"], json!("recall::relevance_weight"));
    // Prior is Beta(7,3): mean = 0.7
    let mean = result["mean"].as_f64().unwrap();
    assert!((mean - 0.7).abs() < 1e-6);
}

// ── Regression tests (issues #355, #356, #357, #295) ──────────────────────

// #356 (MAJ-003): profile_record.total_events must stay in sync with
// balanced_recall.total_events via BOTH the handle_feedback path AND the
// on_dispatch hook path.  The previous fix only wired the sync helper; this
// test pins that removing EITHER call would be caught.
//
// Part A — handle_feedback path (unchanged from before).
// Part B — on_dispatch path: introduce a deliberate desync by reaching into
//   the live state directly, then fire on_dispatch to verify the sync
//   corrects it.  This would fail if sync_balanced_recall_record is removed
//   from on_dispatch.
#[tokio::test]
async fn test_356_profile_record_total_events_synced_after_feedback() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    // Part A: handle_feedback path.
    for _ in 0..3 {
        pack.dispatch(
            "brain.feedback",
            json!({"target_id": target, "signal": "useful"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    }

    let snap = pack.snapshot();
    let live_total = snap.balanced_recall.total_events;

    let record_result = pack
        .dispatch(
            "brain.profile",
            json!({"id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    let record_total = record_result["total_events"].as_u64().unwrap();

    assert_eq!(
        live_total, record_total,
        "#356 part-A: profile_record.total_events ({record_total}) must equal \
         balanced_recall.total_events ({live_total}) after feedback calls"
    );
    assert_eq!(live_total, 3, "expected exactly 3 events from part A");

    // Part B: on_dispatch path.
    // Deliberately desync the record by bumping balanced_recall.total_events
    // directly (simulating what would happen if only on_dispatch updated the
    // live state but the sync call were missing).
    {
        let mut state = pack.state.lock().unwrap();
        state.balanced_recall.total_events += 7; // introduce desync
                                                 // Profile record still says `live_total` at this point.
    }

    // Fire on_dispatch with an irrelevant (non-brain) verb event — this is
    // exactly what the runtime hook does for every non-brain verb dispatch.
    // The sync helper inside on_dispatch must correct the desync.
    let hook_event = {
        use khive_types::{EventKind, SubstrateKind};
        let mut e = khive_storage::event::Event::new(
            "local",
            "search",
            EventKind::Audit,
            SubstrateKind::Event,
            "kg",
        );
        e.outcome = khive_types::EventOutcome::Success;
        e
    };
    let hook_view = khive_runtime::EventView {
        event: hook_event,
        observations: Vec::new(),
    };
    pack.on_dispatch(&hook_view).await;

    // After on_dispatch, the record must reflect the new (desynced) live total.
    let after_hook = pack
        .dispatch(
            "brain.profile",
            json!({"id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    let after_total = after_hook["total_events"].as_u64().unwrap();
    let live_after = pack.snapshot().balanced_recall.total_events;
    assert_eq!(
        after_total, live_after,
        "#356 part-B: on_dispatch sync must correct desync; \
         record shows {after_total}, live state shows {live_after}"
    );
}

// #357 (MAJ-004): brain.feedback must NOT double-count total_events.
//
// The double-count path: VerbRegistry::dispatch calls the registered pack
// handler (handle_feedback folds once) and then calls on_dispatch on every
// registered hook — including BrainPack itself.  Without the brain.* guard
// in on_dispatch, the hook fires a second fold.reduce, making total_events
// == 2.  This test replicates that exact sequence so the test FAILS if the
// guard is absent.
#[tokio::test]
async fn test_357_feedback_no_double_count() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    // Step 1: handle_feedback path — folds once, total_events becomes 1.
    pack.dispatch(
        "brain.feedback",
        json!({"target_id": target, "signal": "useful"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    assert_eq!(
        pack.snapshot().balanced_recall.total_events,
        1,
        "#357 pre-hook: handle_feedback must fold exactly once"
    );

    // Step 2: simulate the registry post-dispatch hook call with a brain.*
    // verb.  This is exactly what VerbRegistry::dispatch does after a
    // successful handler return.  The guard in on_dispatch must return
    // early here — without it, fold.reduce fires again → total_events = 2.
    let hook_event = {
        use khive_types::{EventKind, SubstrateKind};
        khive_storage::event::Event::new(
            "local",
            "brain.feedback",
            EventKind::FeedbackExplicit,
            SubstrateKind::Event,
            "brain",
        )
    };
    let hook_view = khive_runtime::EventView {
        event: hook_event,
        observations: Vec::new(),
    };
    pack.on_dispatch(&hook_view).await;

    assert_eq!(
        pack.snapshot().balanced_recall.total_events,
        1,
        "#357: total_events must remain 1 after on_dispatch(brain.feedback); \
         guard absent if this reads 2"
    );
}

// #295: brain.reset must restore domain-informed priors, not Beta(1,1).
//
// Strengthened per codex P12 Medium: this test now exercises the full
// production path — handle_reset → reset_posteriors → sync helper — and
// verifies that ALL three profile record fields (total_events,
// exploration_epoch, state_snapshot) reflect the restored priors.
//
// It also creates a stale record via hook-only updates (bypassing
// handle_feedback) before the reset, so the desync is real and not
// incidentally corrected by the feedback path.
#[tokio::test]
async fn test_295_reset_restores_domain_priors_not_uniform() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    // Step 0: trigger namespace load before any direct state mutations or hook fires,
    // so ensure_loaded does not reset state after hook events are accumulated.
    // (BRAIN-AUD-001 fix: ensure_loaded resets to a fresh state for any unloaded namespace.)
    pack.dispatch("brain.profiles", json!({}), &registry, &token)
        .await
        .expect("trigger namespace load");

    // Step 1: accumulate state via hook-only updates (no handle_feedback).
    // This simulates the common case where brain observes external pack
    // events rather than explicit feedback calls.
    let hook_event = |verb: &str| {
        use khive_types::{EventKind, SubstrateKind};
        let mut e = khive_storage::event::Event::new(
            "local",
            verb,
            EventKind::Audit,
            SubstrateKind::Event,
            "kg",
        );
        e.outcome = khive_types::EventOutcome::Success;
        e
    };

    // Fire 4 hook events for a non-brain verb (simulates external recall/search).
    for _ in 0..4 {
        let view = khive_runtime::EventView {
            event: hook_event("search"),
            observations: Vec::new(),
        };
        pack.on_dispatch(&view).await;
    }

    // Step 2: also call handle_feedback directly to move salience away from prior.
    // C4: create a real entity so target_id validation passes.
    let target = create_test_entity(&rt, &token).await;
    for _ in 0..5 {
        pack.dispatch(
            "brain.feedback",
            json!({"target_id": target, "signal": "useful"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    }

    // Verify state before reset: posteriors have moved, total_events > 0.
    let before = pack.snapshot();
    assert!(
        before.balanced_recall.salience.alpha() > 2.0,
        "salience.alpha() must have grown past prior after useful feedback"
    );
    assert!(
        before.balanced_recall.total_events >= 9,
        "expected at least 9 total events (4 hook + 5 feedback), got {}",
        before.balanced_recall.total_events
    );
    // Verify record was kept in sync before reset (both paths called sync helper).
    let pre_reset_record = pack
        .dispatch(
            "brain.profile",
            json!({"id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(
        pre_reset_record["total_events"].as_u64().unwrap(),
        before.balanced_recall.total_events,
        "#295 pre-reset: profile record total_events out of sync before reset"
    );

    // Step 3: call handle_reset via the production path (dispatch → handle_reset).
    let reset_result = pack
        .dispatch(
            "brain.reset",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(reset_result["reset"], json!(true));

    // Verify exploration_epoch incremented (reset_posteriors contract).
    let epoch_after = reset_result["exploration_epoch"].as_u64().unwrap();
    assert!(
        epoch_after > 0,
        "#295: exploration_epoch must increment after reset"
    );

    // Step 4: after reset, posteriors must be domain-informed priors — NOT Beta(1,1).
    let after = pack.snapshot();

    // salience prior = Beta(2,8)
    assert!(
        (after.balanced_recall.salience.alpha() - 2.0).abs() < 1e-12,
        "#295: salience.alpha() must be 2.0 after reset, got {}",
        after.balanced_recall.salience.alpha()
    );
    assert!(
        (after.balanced_recall.salience.beta() - 8.0).abs() < 1e-12,
        "#295: salience.beta() must be 8.0 after reset, got {}",
        after.balanced_recall.salience.beta()
    );

    // temporal prior = Beta(1,9)
    assert!(
        (after.balanced_recall.temporal.alpha() - 1.0).abs() < 1e-12,
        "#295: temporal.alpha() must be 1.0 after reset, got {}",
        after.balanced_recall.temporal.alpha()
    );
    assert!(
        (after.balanced_recall.temporal.beta() - 9.0).abs() < 1e-12,
        "#295: temporal.beta() must be 9.0 after reset, got {}",
        after.balanced_recall.temporal.beta()
    );

    // relevance prior = Beta(7,3)
    assert!(
        (after.balanced_recall.relevance.alpha() - 7.0).abs() < 1e-12,
        "#295: relevance.alpha() must be 7.0 after reset"
    );

    // Step 5: brain.profile must reflect the reset state — ALL three fields.
    // This pins the sync_balanced_recall_record call inside handle_reset.
    // Removing that call would cause this assertion to fail.
    let record = pack
        .dispatch(
            "brain.profile",
            json!({"id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();

    // total_events: after reset the state is a fresh BalancedRecallState
    // (total_events = 0), so the record must reflect that.
    let record_total = record["total_events"].as_u64().unwrap();
    assert_eq!(
        record_total, after.balanced_recall.total_events,
        "#295: profile record total_events ({record_total}) must match \
         live state ({}) after reset",
        after.balanced_recall.total_events
    );

    // exploration_epoch: record must match the live state.
    let record_epoch = record["exploration_epoch"].as_u64().unwrap();
    assert_eq!(
        record_epoch, epoch_after,
        "#295: profile record exploration_epoch ({record_epoch}) must match \
         reset result ({epoch_after})"
    );

    // state_snapshot: salience.alpha() must be the prior value.
    let snap = &record["state_snapshot"];
    let sal_alpha = snap["salience"]["alpha"].as_f64().unwrap();
    assert!(
        (sal_alpha - 2.0).abs() < 1e-12,
        "#295: brain.profile state_snapshot salience.alpha() must be 2.0 after reset, \
         got {sal_alpha}"
    );
}

// Round-4 fix: brain.reset must reject unknown kwargs (deny_unknown_fields).
#[tokio::test]
async fn brain_reset_rejects_unknown_kwargs() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let err = pack
        .dispatch(
            "brain.reset",
            json!({"unknownkw": "oops"}),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "brain.reset with unknown kwargs must return InvalidInput, got: {err:?}"
    );
    if let RuntimeError::InvalidInput(msg) = &err {
        assert!(
            msg.contains("brain.reset"),
            "error message must mention brain.reset, got: {msg}"
        );
    }
}

// brain.reset with an empty params object must still succeed.
#[tokio::test]
async fn brain_reset_accepts_empty_params() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let result = pack
        .dispatch("brain.reset", json!({}), &registry, &token)
        .await
        .expect("brain.reset() must succeed with empty params");
    assert_eq!(result["reset"], json!(true));
}

// #355 (regression — real dispatch path): temporal posterior must update
// when a recall hits via the on_dispatch hook carrying real hit/latency.
//
// This test exercises the production wiring added in the P12 codex fix:
// the runtime now embeds duration_us + target_id in the hook event for
// "recall" verbs.  Simulates that by constructing the hook event the way
// the runtime now would, then verifies temporal.alpha() increments.
#[tokio::test]
async fn test_355_posteriors_update_after_dispatch_via_hook() {
    let (pack, _rt) = make_pack();
    let before = pack.snapshot();
    let tmp_alpha_before = before.balanced_recall.temporal.alpha();
    let tmp_beta_before = before.balanced_recall.temporal.beta();

    // Simulate the runtime hook event for a fast recall hit:
    // duration_us ≤ 50_000 (fast) and target_id is present (hit).
    let target_id = uuid::Uuid::new_v4();
    let fast_hit_event = {
        use khive_types::{EventKind, SubstrateKind};
        let mut e = khive_storage::event::Event::new(
            "local",
            "recall",
            EventKind::Audit,
            SubstrateKind::Event,
            "memory",
        );
        e.outcome = khive_types::EventOutcome::Success;
        e.target_id = Some(target_id);
        e.duration_us = 10_000; // 10 ms — fast hit
        e
    };
    let view = khive_runtime::EventView {
        event: fast_hit_event,
        observations: Vec::new(),
    };
    pack.on_dispatch(&view).await;

    let after_fast = pack.snapshot();
    assert!(
        (after_fast.balanced_recall.temporal.alpha() - (tmp_alpha_before + 1.0)).abs() < 1e-12,
        "#355: fast recall hit must increment temporal.alpha() via hook: expected {}, got {}",
        tmp_alpha_before + 1.0,
        after_fast.balanced_recall.temporal.alpha()
    );
    assert!(
        (after_fast.balanced_recall.temporal.beta() - tmp_beta_before).abs() < 1e-12,
        "#355: fast hit must NOT increment temporal.beta()"
    );

    // Simulate a slow recall hit (duration_us > 50_000) → temporal failure.
    let slow_hit_event = {
        use khive_types::{EventKind, SubstrateKind};
        let mut e = khive_storage::event::Event::new(
            "local",
            "recall",
            EventKind::Audit,
            SubstrateKind::Event,
            "memory",
        );
        e.outcome = khive_types::EventOutcome::Success;
        e.target_id = Some(target_id);
        e.duration_us = 100_000; // 100 ms — slow
        e
    };
    let view2 = khive_runtime::EventView {
        event: slow_hit_event,
        observations: Vec::new(),
    };
    pack.on_dispatch(&view2).await;

    let after_slow = pack.snapshot();
    assert!(
        (after_slow.balanced_recall.temporal.beta() - (tmp_beta_before + 1.0)).abs() < 1e-12,
        "#355: slow recall hit must increment temporal.beta() via hook: expected {}, got {}",
        tmp_beta_before + 1.0,
        after_slow.balanced_recall.temporal.beta()
    );

    // Simulate a recall miss (no target_id) → temporal failure.
    let miss_event = {
        use khive_types::{EventKind, SubstrateKind};
        let mut e = khive_storage::event::Event::new(
            "local",
            "recall",
            EventKind::Audit,
            SubstrateKind::Event,
            "memory",
        );
        e.outcome = khive_types::EventOutcome::Success;
        // target_id = None → RecallMiss
        e
    };
    let view3 = khive_runtime::EventView {
        event: miss_event,
        observations: Vec::new(),
    };
    pack.on_dispatch(&view3).await;

    let after_miss = pack.snapshot();
    assert!(
        (after_miss.balanced_recall.temporal.beta() - (tmp_beta_before + 2.0)).abs() < 1e-12,
        "#355: recall miss must further increment temporal.beta(): expected {}, got {}",
        tmp_beta_before + 2.0,
        after_miss.balanced_recall.temporal.beta()
    );
}

// ── Wave-4 Critical regressions (C1-C4) ──────────────────────────────────

// C2: brain.unbind with zero filters must be rejected.
#[tokio::test]
async fn w4_c2_unbind_no_filter_is_rejected() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    // Add a binding so there is something to accidentally wipe.
    pack.dispatch(
        "brain.bind",
        json!({"profile_id": "balanced-recall-v1", "actor": "agent-z", "consumer_kind": "recall"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    let err = pack
        .dispatch("brain.unbind", json!({}), &registry, &token)
        .await
        .unwrap_err();
    if let RuntimeError::InvalidInput(msg) = &err {
        assert!(
            msg.contains("filter") || msg.contains("profile_id") || msg.contains("actor"),
            "C2: zero-filter unbind must mention required filter; got: {msg}"
        );
    } else {
        panic!("C2: zero-filter unbind must return InvalidInput, got {err:?}");
    }

    // Binding must still be intact.
    let state = pack.state.lock().unwrap();
    assert!(
        !state.bindings.is_empty(),
        "C2: binding must survive the rejected unbind"
    );
}

// C3: brain.bind must reject archived profiles.
#[tokio::test]
async fn w4_c3_bind_archived_profile_is_rejected() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    // Archive the only profile.
    pack.dispatch(
        "brain.deactivate",
        json!({"profile_id": "balanced-recall-v1"}),
        &registry,
        &token,
    )
    .await
    .unwrap();
    pack.dispatch(
        "brain.archive",
        json!({"profile_id": "balanced-recall-v1"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    let err = pack
        .dispatch(
            "brain.bind",
            json!({"profile_id": "balanced-recall-v1", "consumer_kind": "recall"}),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    if let RuntimeError::InvalidInput(msg) = &err {
        assert!(
            msg.contains("archived"),
            "C3: bind to archived profile must mention 'archived'; got: {msg}"
        );
    } else {
        panic!("C3: bind to archived profile must return InvalidInput, got {err:?}");
    }
}

// C3: brain.resolve must skip bindings pointing at archived profiles.
#[tokio::test]
async fn w4_c3_resolve_skips_archived_binding() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    // Trigger ensure_loaded for this namespace first, so the subsequent direct
    // state mutation is not overwritten when dispatch calls ensure_loaded again.
    // (BRAIN-AUD-001 fix: ensure_loaded resets state for any unloaded namespace.)
    pack.dispatch("brain.profiles", json!({}), &registry, &token)
        .await
        .expect("trigger namespace load");

    // Force a binding directly into state (bypassing handle_bind guard) to simulate
    // a pre-existing binding that was created before the profile was archived.
    {
        let mut state = pack.state.lock().unwrap();
        state.bindings.push(khive_brain_core::ProfileBinding {
            actor: "*".into(),
            namespace: "*".into(),
            consumer_kind: "recall".into(),
            profile_id: "balanced-recall-v1".into(),
            priority: 100,
            created_at: chrono::Utc::now(),
        });
        // Archive the profile in-state.
        state
            .profiles
            .get_mut("balanced-recall-v1")
            .unwrap()
            .lifecycle = khive_brain_core::ProfileLifecycle::Archived;
    }

    // Resolve must NOT return the archived profile.
    let err = pack
        .dispatch(
            "brain.resolve",
            json!({"consumer_kind": "recall"}),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::NotFound(_)),
        "C3: resolve with only archived binding must return NotFound, got {err:?}"
    );
}

// C4: brain.feedback must reject nonexistent target_id.
#[tokio::test]
async fn w4_c4_feedback_rejects_nonexistent_target() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    let err = pack
        .dispatch(
            "brain.feedback",
            json!({"target_id": "00000000-0000-0000-0000-000000000000", "signal": "useful"}),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::NotFound(_)),
        "C4: feedback with nonexistent target_id must return NotFound, got {err:?}"
    );
}

// C4: brain.feedback must reject nonexistent served_by_profile_id.
#[tokio::test]
async fn w4_c4_feedback_rejects_nonexistent_profile() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    // Create a real entity for the valid target_id.
    let target = create_test_entity(&rt, &token).await;

    let err = pack
        .dispatch(
            "brain.feedback",
            json!({"target_id": target, "signal": "useful", "served_by_profile_id": "fake-profile-xyz"}),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::NotFound(_)),
        "C4: feedback with nonexistent served_by_profile_id must return NotFound, got {err:?}"
    );
}

// C4: brain.feedback with valid target and known profile must succeed.
#[tokio::test]
async fn w4_c4_feedback_accepts_valid_target_and_profile() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    let target = create_test_entity(&rt, &token).await;

    let result = pack
        .dispatch(
            "brain.feedback",
            json!({"target_id": target, "signal": "useful", "served_by_profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(result["emitted"], json!(true));
    assert_eq!(result["signal"], json!("useful"));
}

// H1: brain.create_profile creates a new inactive profile.
#[tokio::test]
async fn w4_h1_create_profile_creates_new_profile() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    let result = pack
        .dispatch(
            "brain.create_profile",
            json!({"name": "my-profile-v1", "consumer_kind": "search", "description": "Custom search profile"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(result["created"], json!(true));
    assert_eq!(result["profile_id"], json!("my-profile-v1"));
    assert_eq!(result["lifecycle"], json!("inactive"));
    assert_eq!(result["consumer_kind"], json!("search"));

    // Verify it appears in brain.profiles.
    let profiles = pack
        .dispatch("brain.profiles", json!({}), &registry, &token)
        .await
        .unwrap();
    let ids: Vec<&str> = profiles["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["id"].as_str())
        .collect();
    assert!(
        ids.contains(&"my-profile-v1"),
        "new profile must appear in brain.profiles"
    );
}

// H1: duplicate name is rejected.
#[tokio::test]
async fn w4_h1_create_profile_duplicate_rejected() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    let err = pack
        .dispatch(
            "brain.create_profile",
            json!({"name": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "H1: duplicate profile name must return InvalidInput, got {err:?}"
    );
}

// H2: brain.bindings lists binding rows.
#[tokio::test]
async fn w4_h2_bindings_lists_rows() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    // Initially empty.
    let result = pack
        .dispatch("brain.bindings", json!({}), &registry, &token)
        .await
        .unwrap();
    assert_eq!(result["count"], json!(0u64));
    assert_eq!(result["bindings"], json!([]));

    // Add a binding.
    pack.dispatch(
        "brain.bind",
        json!({"profile_id": "balanced-recall-v1", "actor": "agent-a", "consumer_kind": "recall"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    let result2 = pack
        .dispatch("brain.bindings", json!({}), &registry, &token)
        .await
        .unwrap();
    assert_eq!(result2["count"], json!(1u64));
    let rows = result2["bindings"].as_array().unwrap();
    assert_eq!(rows[0]["actor"], json!("agent-a"));
    assert_eq!(rows[0]["profile_id"], json!("balanced-recall-v1"));
}

// H2: brain.bindings supports filtering.
#[tokio::test]
async fn w4_h2_bindings_filtered() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    pack.dispatch(
        "brain.bind",
        json!({"profile_id": "balanced-recall-v1", "actor": "agent-1", "consumer_kind": "recall"}),
        &registry,
        &token,
    )
    .await
    .unwrap();
    pack.dispatch(
        "brain.bind",
        json!({"profile_id": "balanced-recall-v1", "actor": "agent-2", "consumer_kind": "search"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    let result = pack
        .dispatch(
            "brain.bindings",
            json!({"actor": "agent-1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(result["count"], json!(1u64));
    assert_eq!(result["bindings"][0]["actor"], json!("agent-1"));
}

// H3: brain.resolve response includes both requested and matched consumer_kind.
#[tokio::test]
async fn w4_h3_resolve_returns_both_requested_and_matched_kind() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    // Install a wildcard binding (consumer_kind = "*").
    pack.dispatch(
        "brain.bind",
        json!({"profile_id": "balanced-recall-v1", "consumer_kind": "*", "priority": 1}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    // Query for a different consumer_kind — wildcard binding matches.
    let result = pack
        .dispatch(
            "brain.resolve",
            json!({"consumer_kind": "search"}),
            &registry,
            &token,
        )
        .await
        .unwrap();

    assert_eq!(
        result["requested_consumer_kind"],
        json!("search"),
        "H3: requested_consumer_kind must equal the query"
    );
    assert_eq!(
        result["matched_consumer_kind"],
        json!("*"),
        "H3: matched_consumer_kind must show the wildcard binding"
    );
    assert_eq!(result["resolved_profile_id"], json!("balanced-recall-v1"));
}

// H3: exact match returns matching kind in matched_consumer_kind.
#[tokio::test]
async fn w4_h3_resolve_exact_match_returns_exact_kind() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    // Default fallback (no binding) uses profile's consumer_kind.
    let result = pack
        .dispatch(
            "brain.resolve",
            json!({"consumer_kind": "recall"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(result["requested_consumer_kind"], json!("recall"));
    assert_eq!(result["matched_consumer_kind"], json!("recall"));
}

// Round-2 fix 3: archived high-priority binding + live lower-priority wildcard → live wins.
#[tokio::test]
async fn r2_archived_exact_binding_defers_to_live_wildcard() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    // Create a second live profile.
    pack.dispatch(
        "brain.create_profile",
        json!({"name": "search-v1", "consumer_kind": "search"}),
        &registry,
        &token,
    )
    .await
    .unwrap();
    pack.dispatch(
        "brain.activate",
        json!({"profile_id": "search-v1"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    // Insert a high-priority exact binding pointing at the default profile.
    {
        let mut state = pack.state.lock().unwrap();
        state.bindings.push(khive_brain_core::ProfileBinding {
            actor: "*".into(),
            namespace: "*".into(),
            consumer_kind: "search".into(),
            profile_id: "balanced-recall-v1".into(),
            priority: 100,
            created_at: chrono::Utc::now(),
        });
        // Archive balanced-recall-v1 in-state to simulate it being retired.
        state
            .profiles
            .get_mut("balanced-recall-v1")
            .unwrap()
            .lifecycle = khive_brain_core::ProfileLifecycle::Archived;
    }

    // Add a lower-priority wildcard binding pointing at the live profile.
    pack.dispatch(
        "brain.bind",
        json!({"profile_id": "search-v1", "consumer_kind": "*", "priority": 1}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    // Resolve must return the live lower-priority profile, NOT fall to default.
    let result = pack
        .dispatch(
            "brain.resolve",
            json!({"consumer_kind": "search"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(
        result["resolved_profile_id"],
        json!("search-v1"),
        "r2 fix 3: archived high-priority binding must not suppress the live wildcard binding"
    );
}

// Round-2 fix 4: brain.feedback rejects archived served_by_profile_id.
#[tokio::test]
async fn r2_feedback_rejects_archived_served_by_profile() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    let target = create_test_entity(&rt, &token).await;

    // Archive the only profile.
    pack.dispatch(
        "brain.deactivate",
        json!({"profile_id": "balanced-recall-v1"}),
        &registry,
        &token,
    )
    .await
    .unwrap();
    pack.dispatch(
        "brain.archive",
        json!({"profile_id": "balanced-recall-v1"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    let err = pack
        .dispatch(
            "brain.feedback",
            json!({
                "target_id": target,
                "signal": "useful",
                "served_by_profile_id": "balanced-recall-v1"
            }),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    if let RuntimeError::InvalidInput(msg) = &err {
        assert!(
            msg.contains("archived"),
            "r2 fix 4: feedback to archived profile must mention 'archived'; got: {msg}"
        );
    } else {
        panic!("r2 fix 4: feedback to archived served_by_profile_id must return InvalidInput, got {err:?}");
    }
}

// Round-2 fix 5: brain.create_profile rejects empty and wildcard consumer_kind.
#[tokio::test]
async fn r2_create_profile_rejects_empty_consumer_kind() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    let err = pack
        .dispatch(
            "brain.create_profile",
            json!({"name": "bad-profile", "consumer_kind": ""}),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "r2 fix 5: empty consumer_kind must return InvalidInput, got {err:?}"
    );
}

#[tokio::test]
async fn r2_create_profile_rejects_wildcard_consumer_kind() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    let err = pack
        .dispatch(
            "brain.create_profile",
            json!({"name": "wildcard-profile", "consumer_kind": "*"}),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    if let RuntimeError::InvalidInput(msg) = &err {
        assert!(
            msg.contains("wildcard") || msg.contains("sentinel") || msg.contains("*"),
            "r2 fix 5: wildcard consumer_kind rejection must explain the issue; got: {msg}"
        );
    } else {
        panic!("r2 fix 5: wildcard consumer_kind must return InvalidInput, got {err:?}");
    }
}

#[tokio::test]
async fn r2_create_profile_rejects_whitespace_consumer_kind() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    let err = pack
        .dispatch(
            "brain.create_profile",
            json!({"name": "ws-profile", "consumer_kind": "   "}),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "r2 fix 5: whitespace consumer_kind must return InvalidInput, got {err:?}"
    );
}

// Round-2 fix 6: brain.bindings AND-semantics pinned with ≥3 bindings and combined filters.
#[tokio::test]
async fn r2_bindings_and_semantics_multi_filter() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    // Create two extra profiles for variety.
    for name in ["alpha-v1", "beta-v1"] {
        pack.dispatch(
            "brain.create_profile",
            json!({"name": name, "consumer_kind": "recall"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
        pack.dispatch(
            "brain.activate",
            json!({"profile_id": name}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    }

    // Three bindings with distinct (actor, namespace, consumer_kind) combos.
    pack.dispatch(
        "brain.bind",
        json!({"profile_id": "balanced-recall-v1", "actor": "agent-A", "namespace": "ns-1", "consumer_kind": "recall"}),
        &registry,
        &token,
    )
    .await
    .unwrap();
    pack.dispatch(
        "brain.bind",
        json!({"profile_id": "alpha-v1", "actor": "agent-A", "namespace": "ns-2", "consumer_kind": "search"}),
        &registry,
        &token,
    )
    .await
    .unwrap();
    pack.dispatch(
        "brain.bind",
        json!({"profile_id": "beta-v1", "actor": "agent-B", "namespace": "ns-1", "consumer_kind": "recall"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    // Filter by profile_id + namespace: should return exactly 1 row.
    let r1 = pack
        .dispatch(
            "brain.bindings",
            json!({"profile_id": "balanced-recall-v1", "namespace": "ns-1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(
        r1["count"],
        json!(1u64),
        "AND filter profile_id+namespace must return 1 row"
    );
    assert_eq!(r1["bindings"][0]["profile_id"], json!("balanced-recall-v1"));
    assert_eq!(r1["bindings"][0]["namespace"], json!("ns-1"));

    // Filter by actor + consumer_kind: agent-A+search → only alpha-v1 row.
    let r2 = pack
        .dispatch(
            "brain.bindings",
            json!({"actor": "agent-A", "consumer_kind": "search"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(
        r2["count"],
        json!(1u64),
        "AND filter actor+consumer_kind must return 1 row"
    );
    assert_eq!(r2["bindings"][0]["profile_id"], json!("alpha-v1"));

    // Zero-row combination: agent-B + consumer_kind=search (no such binding).
    let r3 = pack
        .dispatch(
            "brain.bindings",
            json!({"actor": "agent-B", "consumer_kind": "search"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(
        r3["count"],
        json!(0u64),
        "AND filter with no matches must return count=0"
    );
    assert_eq!(r3["bindings"], json!([]));
}

// Round-2 fix 2: user-created profile has real posterior state that reset mutates.
// Round-3 strengthening: emit feedback first to move posteriors, then assert all three
// posterior alpha/beta values return to their priors after reset.
#[tokio::test]
async fn r2_user_profile_reset_mutates_posteriors() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    let target = create_test_entity(&rt, &token).await;

    pack.dispatch(
        "brain.create_profile",
        json!({"name": "custom-v1", "consumer_kind": "recall"}),
        &registry,
        &token,
    )
    .await
    .unwrap();
    pack.dispatch(
        "brain.activate",
        json!({"profile_id": "custom-v1"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    // Emit feedback to custom-v1 so its salience posterior diverges from prior.
    // brain.feedback with signal="useful" → salience.update_success() (fold.rs:54).
    pack.dispatch(
        "brain.feedback",
        json!({"target_id": target, "signal": "useful", "served_by_profile_id": "custom-v1"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    // Confirm salience.alpha() increased above the prior (2.0).
    let mutated = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": "custom-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    let salience_alpha_before = mutated["state_snapshot"]["salience"]["alpha"]
        .as_f64()
        .expect("state_snapshot.salience.alpha() must be a number");
    assert!(
        salience_alpha_before > 2.0,
        "r3 fix 2: feedback must have moved salience alpha above prior 2.0; got {salience_alpha_before}"
    );
    let epoch_before = mutated["exploration_epoch"].as_u64().unwrap();

    // Reset custom profile.
    let reset_result = pack
        .dispatch(
            "brain.reset",
            json!({"profile_id": "custom-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(reset_result["reset"], json!(true));
    assert_eq!(reset_result["profile_id"], json!("custom-v1"));

    // Epoch must increment.
    let after = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": "custom-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    let epoch_after = after["exploration_epoch"].as_u64().unwrap();
    assert!(
        epoch_after > epoch_before,
        "r3 fix 2: reset must increment exploration_epoch on user-created profile; before={epoch_before} after={epoch_after}"
    );

    // All three posteriors must return exactly to priors:
    //   relevance  = Beta(7, 3)
    //   salience   = Beta(2, 8)
    //   temporal   = Beta(1, 9)
    let snap = &after["state_snapshot"];
    assert!(
        !snap.is_null(),
        "r3 fix 2: state_snapshot must be non-null after reset"
    );

    let rel_alpha = snap["relevance"]["alpha"]
        .as_f64()
        .expect("relevance.alpha()");
    let rel_beta = snap["relevance"]["beta"]
        .as_f64()
        .expect("relevance.beta()");
    assert!(
        (rel_alpha - 7.0).abs() < 1e-9 && (rel_beta - 3.0).abs() < 1e-9,
        "r3 fix 2: relevance must be Beta(7,3) after reset; got ({rel_alpha},{rel_beta})"
    );

    let sal_alpha = snap["salience"]["alpha"]
        .as_f64()
        .expect("salience.alpha()");
    let sal_beta = snap["salience"]["beta"].as_f64().expect("salience.beta()");
    assert!(
        (sal_alpha - 2.0).abs() < 1e-9 && (sal_beta - 8.0).abs() < 1e-9,
        "r3 fix 2: salience must be Beta(2,8) after reset; got ({sal_alpha},{sal_beta})"
    );

    let tmp_alpha = snap["temporal"]["alpha"]
        .as_f64()
        .expect("temporal.alpha()");
    let tmp_beta = snap["temporal"]["beta"].as_f64().expect("temporal.beta()");
    assert!(
        (tmp_alpha - 1.0).abs() < 1e-9 && (tmp_beta - 9.0).abs() < 1e-9,
        "r3 fix 2: temporal must be Beta(1,9) after reset; got ({tmp_alpha},{tmp_beta})"
    );
}

// Round-2 fix 2: feedback routes to the user-created profile's own state.
#[tokio::test]
async fn r2_user_profile_feedback_routes_to_profile_state() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    let target = create_test_entity(&rt, &token).await;

    pack.dispatch(
        "brain.create_profile",
        json!({"name": "custom-v1", "consumer_kind": "recall"}),
        &registry,
        &token,
    )
    .await
    .unwrap();
    pack.dispatch(
        "brain.activate",
        json!({"profile_id": "custom-v1"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    let before = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": "custom-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    let events_before = before["total_events"].as_u64().unwrap();

    pack.dispatch(
        "brain.feedback",
        json!({"target_id": target, "signal": "useful", "served_by_profile_id": "custom-v1"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    let after = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": "custom-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    let events_after = after["total_events"].as_u64().unwrap();
    assert!(
        events_after > events_before,
        "r2 fix 2: feedback routed to custom profile must increment its total_events; before={events_before} after={events_after}"
    );
}

// H4: brain.profile accepts profile_id (canonical) and id (alias).
#[tokio::test]
async fn w4_h4_profile_accepts_profile_id_and_id_alias() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    // Canonical arg.
    let r1 = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(r1["id"], json!("balanced-recall-v1"));

    // Legacy alias.
    let r2 = pack
        .dispatch(
            "brain.profile",
            json!({"id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(r2["id"], json!("balanced-recall-v1"));
}

// ── Round-3 regression tests ──────────────────────────────────────────────

// R3-1: archiving balanced-recall-v1 then calling brain.feedback without
// served_by_profile_id must return InvalidInput and must NOT append an event.
#[tokio::test]
async fn r3_feedback_default_profile_archived_rejected() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    let target = create_test_entity(&rt, &token).await;

    // balanced-recall-v1 starts Active; must deactivate before archiving (lifecycle rule).
    pack.dispatch(
        "brain.deactivate",
        json!({"profile_id": "balanced-recall-v1"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    // Archive the default profile.
    pack.dispatch(
        "brain.archive",
        json!({"profile_id": "balanced-recall-v1"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    // Baseline: total_events on default profile before the attempted feedback.
    // Also capture brain.events count so we catch any FeedbackExplicit row that
    // sneaks past the lifecycle check via a reordered append → fold sequence.
    let snap_before = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    let events_before = snap_before["total_events"].as_u64().unwrap_or(0);

    let log_before = pack
        .dispatch("brain.events", json!({"limit": 1000}), &registry, &token)
        .await
        .unwrap();
    let log_count_before = log_before["events"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);

    // feedback without served_by_profile_id must be rejected.
    let err = pack
        .dispatch(
            "brain.feedback",
            json!({"target_id": target, "signal": "useful"}),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    match &err {
        RuntimeError::InvalidInput(msg) => {
            assert!(
                msg.contains("archived"),
                "r3-1: error must mention 'archived'; got: {msg}"
            );
        }
        other => panic!("r3-1: expected InvalidInput(archived), got {other:?}"),
    }

    // No state mutation: total_events must be unchanged.
    let snap_after = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    let events_after = snap_after["total_events"].as_u64().unwrap_or(0);
    assert_eq!(
        events_after, events_before,
        "r3-1: archived default profile must not have events appended; before={events_before} after={events_after}"
    );

    // Defense-in-depth: also verify nothing landed in the event log itself.
    // A future reorder that appends FeedbackExplicit before the lifecycle check
    // but skips the fold would leave total_events unchanged but still write to
    // the log. This assertion catches that class.
    let log_after = pack
        .dispatch("brain.events", json!({"limit": 1000}), &registry, &token)
        .await
        .unwrap();
    let log_count_after = log_after["events"].as_array().map(|a| a.len()).unwrap_or(0);
    assert_eq!(
        log_count_after, log_count_before,
        "r3-1: rejected feedback must not append a FeedbackExplicit event; before={log_count_before} after={log_count_after}"
    );
}

// R3-3: brain.create_profile profile-id grammar enforcement.
#[tokio::test]
async fn r3_create_profile_id_grammar_enforced() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    // Whitespace-only name must be rejected.
    let err = pack
        .dispatch(
            "brain.create_profile",
            json!({"name": "   ", "consumer_kind": "recall"}),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "r3-3: whitespace-only name must return InvalidInput; got {err:?}"
    );

    // Leading/trailing space: trimmed name "my-profile" must be accepted.
    pack.dispatch(
        "brain.create_profile",
        json!({"name": "  my-profile  ", "consumer_kind": "recall"}),
        &registry,
        &token,
    )
    .await
    .expect("r3-3: name with leading/trailing spaces should be accepted after trim");

    // Dot in name must be rejected.
    let err = pack
        .dispatch(
            "brain.create_profile",
            json!({"name": "bad.profile", "consumer_kind": "recall"}),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "r3-3: dot in name must return InvalidInput; got {err:?}"
    );

    // Underscore in name must be rejected.
    let err = pack
        .dispatch(
            "brain.create_profile",
            json!({"name": "bad_profile", "consumer_kind": "recall"}),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "r3-3: underscore in name must return InvalidInput; got {err:?}"
    );

    // Asterisk in name must be rejected.
    let err = pack
        .dispatch(
            "brain.create_profile",
            json!({"name": "*", "consumer_kind": "recall"}),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "r3-3: asterisk name must return InvalidInput; got {err:?}"
    );

    // Valid alphanumeric-hyphen name must succeed.
    pack.dispatch(
        "brain.create_profile",
        json!({"name": "valid-profile-123", "consumer_kind": "recall"}),
        &registry,
        &token,
    )
    .await
    .expect("r3-3: valid alphanumeric-hyphen name must succeed");
}

// #289: feedback event must record a non-zero duration_us.
#[tokio::test]
async fn test_289_feedback_event_records_nonzero_duration() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    let result = pack
        .dispatch(
            "brain.feedback",
            json!({"target_id": target, "signal": "useful"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    let event_id = result["event_id"].as_str().unwrap().to_string();

    let log = pack
        .dispatch("brain.events", json!({"limit": 100}), &registry, &token)
        .await
        .unwrap();
    let event = log["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"].as_str() == Some(event_id.as_str()))
        .expect("#289: feedback event must appear in brain.events");

    assert!(
        event["duration_us"].as_i64().unwrap() > 0,
        "#289: feedback event duration_us must be non-zero, got {}",
        event["duration_us"]
    );
}

// ── #517: brain.auto_feedback ─────────────────────────────────────────────

#[tokio::test]
async fn brain_auto_feedback_emits_implicit_positive_for_first_result() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    let result = pack
        .dispatch(
            "brain.auto_feedback",
            json!({
                "query": "recall calibration target",
                "results": [{ "note_id": target }]
            }),
            &registry,
            &token,
        )
        .await
        .expect("auto_feedback succeeds");

    assert_eq!(result["emitted"], json!(true), "emitted must be true");
    assert_eq!(
        result["signal"],
        json!("implicit_positive"),
        "default signal must be implicit_positive"
    );
    let returned_target_id = result["target_id"].as_str().unwrap_or("");
    assert_eq!(
        returned_target_id.len(),
        36,
        "target_id in auto_feedback response must be full 36-char UUID"
    );
    assert_eq!(
        returned_target_id, target,
        "target_id must match the created entity"
    );
    assert_eq!(
        pack.snapshot().balanced_recall.total_events,
        1,
        "auto_feedback must increment total_events"
    );
}

#[tokio::test]
async fn brain_auto_feedback_empty_results_returns_no_emit() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    let result = pack
        .dispatch(
            "brain.auto_feedback",
            json!({
                "query": "empty recall results",
                "results": []
            }),
            &registry,
            &token,
        )
        .await
        .expect("auto_feedback with empty results succeeds");

    assert_eq!(result["emitted"], json!(false));
    assert_eq!(result["reason"], json!("no_results"));
}

#[tokio::test]
async fn brain_auto_feedback_accepts_short_note_id_prefix() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;
    // Use 8-char prefix as Agent mode would return from memory.recall.
    let prefix = &target[..8];

    let result = pack
        .dispatch(
            "brain.auto_feedback",
            json!({
                "query": "prefix resolution test",
                "results": [{ "note_id": prefix }]
            }),
            &registry,
            &token,
        )
        .await
        .expect("auto_feedback with 8-char prefix succeeds");

    assert_eq!(result["emitted"], json!(true));
    assert_eq!(result["target_id"].as_str().unwrap_or("").len(), 36);
}

// BRAIN-AUD-001: verify namespace isolation — state from namespace A must
// not be visible when dispatching under namespace B.
#[tokio::test]
async fn namespace_isolation_state_does_not_leak() {
    use core::convert::TryFrom;
    use khive_runtime::Namespace;

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let pack = BrainPack::new(rt.clone());
    let registry = empty_registry();

    let ns_a = Namespace::try_from("ns-a").expect("namespace a");
    let ns_b = Namespace::try_from("ns-b").expect("namespace b");
    let token_a = rt.authorize(ns_a).expect("token a");
    let token_b = rt.authorize(ns_b).expect("token b");

    // Create a profile binding in namespace A.
    pack.dispatch(
        "brain.bind",
        json!({
            "profile_id": "balanced-recall-v1",
            "actor": "alice-a",
            "namespace": "ns-a",
            "consumer_kind": "recall",
        }),
        &registry,
        &token_a,
    )
    .await
    .expect("bind in namespace A");

    // Dispatch under namespace B — it should see a fresh state with no bindings.
    let bindings_b = pack
        .dispatch("brain.bindings", json!({}), &registry, &token_b)
        .await
        .expect("bindings in namespace B");

    assert_eq!(
        bindings_b["count"],
        json!(0u64),
        "namespace B must see 0 bindings; got: {bindings_b}"
    );

    // Also verify profiles list in namespace B shows only the built-in default.
    let profiles_b = pack
        .dispatch("brain.profiles", json!({}), &registry, &token_b)
        .await
        .expect("profiles in namespace B");

    let count_b = profiles_b["count"].as_u64().unwrap_or(0);
    let profiles_a = pack
        .dispatch("brain.profiles", json!({}), &registry, &token_a)
        .await
        .expect("profiles in namespace A");
    let count_a = profiles_a["count"].as_u64().unwrap_or(0);

    // Both namespaces see the same built-in default (not each other's custom data).
    assert_eq!(
        count_b, count_a,
        "both namespaces must see only the built-in default profile"
    );
}

#[cfg(test)]
mod help_tests {
    use super::*;
    use crate::handlers::BRAIN_HANDLERS;

    fn find_handler(name: &str) -> &'static HandlerDef {
        BRAIN_HANDLERS
            .iter()
            .find(|h| h.name == name)
            .unwrap_or_else(|| panic!("handler {name:?} not found in BRAIN_HANDLERS"))
    }

    #[test]
    fn brain_feedback_params_non_empty_and_has_target_and_signal() {
        let h = find_handler("brain.feedback");
        assert!(!h.params.is_empty(), "brain.feedback must have params");
        assert!(
            h.params.iter().any(|p| p.name == "target_id" && p.required),
            "brain.feedback must have required target_id param"
        );
        assert!(
            h.params.iter().any(|p| p.name == "signal" && p.required),
            "brain.feedback must have required signal param"
        );
        assert!(
            h.params.iter().any(|p| p.name == "served_by_profile_id"),
            "brain.feedback must document served_by_profile_id"
        );
    }

    #[test]
    fn brain_auto_feedback_handler_is_declared() {
        let h = find_handler("brain.auto_feedback");
        assert!(
            h.params.iter().any(|p| p.name == "query" && p.required),
            "brain.auto_feedback must have required query param"
        );
        assert!(
            h.params.iter().any(|p| p.name == "results" && p.required),
            "brain.auto_feedback must have required results param"
        );
    }

    #[test]
    fn brain_profile_params_has_required_profile_id() {
        let h = find_handler("brain.profile");
        assert!(!h.params.is_empty(), "brain.profile must have params");
        assert!(
            h.params
                .iter()
                .any(|p| p.name == "profile_id" && p.required),
            "brain.profile must have required profile_id param (H4 fix)"
        );
    }

    #[test]
    fn brain_profiles_params_has_lifecycle_filter() {
        let h = find_handler("brain.profiles");
        assert!(!h.params.is_empty(), "brain.profiles must have params");
        assert!(
            h.params.iter().any(|p| p.name == "lifecycle"),
            "brain.profiles must document lifecycle filter param"
        );
    }

    #[test]
    fn brain_resolve_params_has_consumer_kind_required() {
        let h = find_handler("brain.resolve");
        assert!(!h.params.is_empty(), "brain.resolve must have params");
        assert!(
            h.params
                .iter()
                .any(|p| p.name == "consumer_kind" && p.required),
            "brain.resolve must have required consumer_kind"
        );
        assert!(
            h.params.iter().any(|p| p.name == "actor"),
            "brain.resolve must document optional actor"
        );
        assert!(
            h.params.iter().any(|p| p.name == "namespace"),
            "brain.resolve must document optional namespace"
        );
    }

    #[test]
    fn brain_bind_params_has_required_profile_id_and_optionals() {
        let h = find_handler("brain.bind");
        assert!(!h.params.is_empty(), "brain.bind must have params");
        assert!(
            h.params
                .iter()
                .any(|p| p.name == "profile_id" && p.required),
            "brain.bind must have required profile_id"
        );
        assert!(
            h.params.iter().any(|p| p.name == "actor"),
            "brain.bind must document actor"
        );
        assert!(
            h.params.iter().any(|p| p.name == "namespace"),
            "brain.bind must document namespace"
        );
        assert!(
            h.params.iter().any(|p| p.name == "consumer_kind"),
            "brain.bind must document consumer_kind"
        );
        assert!(
            h.params.iter().any(|p| p.name == "priority"),
            "brain.bind must document priority"
        );
    }

    #[test]
    fn brain_unbind_params_non_empty_all_optional() {
        let h = find_handler("brain.unbind");
        assert!(!h.params.is_empty(), "brain.unbind must have params");
        assert!(
            h.params.iter().all(|p| !p.required),
            "brain.unbind params must all be optional (filter semantics)"
        );
        assert!(
            h.params.iter().any(|p| p.name == "profile_id"),
            "brain.unbind must document profile_id filter"
        );
        assert!(
            h.params.iter().any(|p| p.name == "actor"),
            "brain.unbind must document actor filter"
        );
    }

    #[test]
    fn brain_activate_deactivate_archive_each_have_profile_id() {
        for verb in ["brain.activate", "brain.deactivate", "brain.archive"] {
            let h = find_handler(verb);
            assert!(!h.params.is_empty(), "{verb} must have params");
            assert!(
                h.params
                    .iter()
                    .any(|p| p.name == "profile_id" && p.required),
                "{verb} must have required profile_id param"
            );
        }
    }

    #[test]
    fn brain_reset_params_has_optional_profile_id() {
        let h = find_handler("brain.reset");
        assert!(!h.params.is_empty(), "brain.reset must have params");
        assert!(
            h.params
                .iter()
                .any(|p| p.name == "profile_id" && !p.required),
            "brain.reset profile_id must be optional (defaults to balanced-recall-v1)"
        );
    }

    #[test]
    fn brain_config_params_has_parameter() {
        let h = find_handler("brain.config");
        assert!(
            !h.params.is_empty(),
            "brain.config must document the parameter arg"
        );
        assert!(
            h.params
                .iter()
                .any(|p| p.name == "parameter" && !p.required),
            "brain.config parameter must be optional"
        );
    }

    #[test]
    fn brain_events_params_has_limit() {
        let h = find_handler("brain.events");
        assert!(
            !h.params.is_empty(),
            "brain.events must document the limit arg"
        );
        assert!(
            h.params.iter().any(|p| p.name == "limit" && !p.required),
            "brain.events limit must be optional"
        );
    }

    #[test]
    fn brain_emit_params_non_empty_with_target_and_signal() {
        let h = find_handler("brain.emit");
        assert!(
            !h.params.is_empty(),
            "brain.emit must have params (mirrors brain.feedback)"
        );
        assert!(
            h.params.iter().any(|p| p.name == "target_id" && p.required),
            "brain.emit must have required target_id"
        );
        assert!(
            h.params.iter().any(|p| p.name == "signal" && p.required),
            "brain.emit must have required signal"
        );
    }

    #[test]
    fn brain_bindings_params_all_optional() {
        let h = find_handler("brain.bindings");
        assert!(
            h.params.iter().all(|p| !p.required),
            "brain.bindings: all params must be optional filter args"
        );
        assert!(
            h.params.iter().any(|p| p.name == "profile_id"),
            "brain.bindings must document profile_id filter"
        );
        assert!(
            h.params.iter().any(|p| p.name == "consumer_kind"),
            "brain.bindings must document consumer_kind filter"
        );
    }

    #[test]
    fn brain_create_profile_params_has_required_name() {
        let h = find_handler("brain.create_profile");
        assert!(
            !h.params.is_empty(),
            "brain.create_profile must have params"
        );
        assert!(
            h.params.iter().any(|p| p.name == "name" && p.required),
            "brain.create_profile must have required name param"
        );
        assert!(
            h.params
                .iter()
                .any(|p| p.name == "consumer_kind" && !p.required),
            "brain.create_profile consumer_kind must be optional"
        );
    }

    // ── Regression: schema-aware namespace strip (codex round-2 H1) ──────────
    //
    // brain.bind / brain.resolve / brain.unbind / brain.bindings declare
    // `namespace` as a *business* parameter in their HandlerDef.params.  The
    // VerbRegistry dispatch path must NOT strip `namespace` from those verbs
    // even though it strips it as a transport routing key from all other verbs.
    //
    // These tests go through VerbRegistry::dispatch (not pack.dispatch) to
    // exercise the actual strip logic in pack.rs.

    /// Build a VerbRegistry with kg + brain packs, returning the registry and
    /// an owned BrainPack snapshot handle.  We need a reference to the brain
    /// state after dispatch — the registry owns the pack, so we verify via a
    /// second dispatch (brain.bindings) rather than peeking at internal state.
    fn make_brain_registry() -> (khive_runtime::VerbRegistry, KhiveRuntime) {
        use khive_pack_kg::KgPack;
        use khive_runtime::VerbRegistryBuilder;
        let rt = KhiveRuntime::memory().expect("in-memory runtime for brain registry");
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(BrainPack::new(rt.clone()));
        let registry = builder.build().expect("kg+brain registry builds");
        (registry, rt)
    }

    /// brain.bind via VerbRegistry must store the caller-supplied namespace,
    /// not default to "*".  Regression for the blanket-strip bug (codex H1).
    #[tokio::test]
    async fn r2_h1_bind_via_registry_preserves_namespace() {
        use serde_json::json;
        let (registry, _rt) = make_brain_registry();

        // Bind with a specific namespace.
        let result = registry
            .dispatch(
                "brain.bind",
                json!({
                    "profile_id": "balanced-recall-v1",
                    "actor": "alice",
                    "namespace": "team-a",
                    "consumer_kind": "recall",
                }),
            )
            .await
            .expect("brain.bind must succeed");
        assert_eq!(
            result["namespace"],
            json!("team-a"),
            "brain.bind response must echo the caller-supplied namespace"
        );

        // Verify via brain.bindings: the stored row must have namespace=team-a.
        let bindings = registry
            .dispatch(
                "brain.bindings",
                json!({
                    "profile_id": "balanced-recall-v1",
                    "namespace": "team-a",
                }),
            )
            .await
            .expect("brain.bindings must succeed");
        assert_eq!(
            bindings["count"],
            json!(1u64),
            "must find exactly one binding for namespace=team-a"
        );
        assert_eq!(
            bindings["bindings"][0]["namespace"],
            json!("team-a"),
            "stored binding namespace must be team-a, not wildcard"
        );
    }

    /// brain.resolve via VerbRegistry must use the caller-supplied namespace to
    /// match the binding stored by brain.bind.  Regression for codex H1.
    #[tokio::test]
    async fn r2_h1_resolve_via_registry_uses_namespace() {
        use serde_json::json;
        let (registry, _rt) = make_brain_registry();

        // Store a binding scoped to team-a.
        registry
            .dispatch(
                "brain.bind",
                json!({
                    "profile_id": "balanced-recall-v1",
                    "actor": "alice",
                    "namespace": "team-a",
                    "consumer_kind": "recall",
                }),
            )
            .await
            .expect("brain.bind team-a");

        // Resolve for alice / team-a / recall — must find balanced-recall-v1.
        let resolved = registry
            .dispatch(
                "brain.resolve",
                json!({
                    "actor": "alice",
                    "namespace": "team-a",
                    "consumer_kind": "recall",
                }),
            )
            .await
            .expect("brain.resolve must succeed for team-a");
        assert_eq!(
            resolved["resolved_profile_id"],
            json!("balanced-recall-v1"),
            "resolve must return the profile bound for team-a"
        );
    }

    /// brain.unbind via VerbRegistry must use the caller-supplied namespace to
    /// remove only the matching binding.  Regression for codex H1.
    #[tokio::test]
    async fn r2_h1_unbind_via_registry_uses_namespace() {
        use serde_json::json;
        let (registry, _rt) = make_brain_registry();

        // Two bindings: team-a and team-b.
        registry
            .dispatch(
                "brain.bind",
                json!({
                    "profile_id": "balanced-recall-v1",
                    "actor": "alice",
                    "namespace": "team-a",
                    "consumer_kind": "recall",
                }),
            )
            .await
            .expect("bind team-a");
        registry
            .dispatch(
                "brain.bind",
                json!({
                    "profile_id": "balanced-recall-v1",
                    "actor": "alice",
                    "namespace": "team-b",
                    "consumer_kind": "recall",
                }),
            )
            .await
            .expect("bind team-b");

        // Unbind only team-a.
        let unbound = registry
            .dispatch(
                "brain.unbind",
                json!({
                    "actor": "alice",
                    "namespace": "team-a",
                }),
            )
            .await
            .expect("unbind team-a");
        assert_eq!(
            unbound["unbound"],
            json!(1u64),
            "must remove exactly one binding (team-a)"
        );

        // team-b must survive.
        let remaining = registry
            .dispatch(
                "brain.bindings",
                json!({
                    "actor": "alice",
                    "namespace": "team-b",
                }),
            )
            .await
            .expect("bindings after unbind");
        assert_eq!(
            remaining["count"],
            json!(1u64),
            "team-b binding must survive the team-a unbind"
        );
    }

    // BRAIN-AUD-005: brain.profiles output order must be deterministic.
    #[tokio::test]
    async fn profiles_output_is_sorted_by_id() {
        use khive_runtime::{Namespace, PackRuntime, VerbRegistryBuilder};
        use serde_json::json;
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let pack = BrainPack::new(rt.clone());
        let registry = VerbRegistryBuilder::new().build().expect("empty registry");
        let token = rt.authorize(Namespace::local()).unwrap();

        // Create multiple profiles so there are several entries to sort.
        pack.dispatch(
            "brain.create_profile",
            json!({ "name": "z-profile" }),
            &registry,
            &token,
        )
        .await
        .expect("create z-profile");

        pack.dispatch(
            "brain.create_profile",
            json!({ "name": "a-profile" }),
            &registry,
            &token,
        )
        .await
        .expect("create a-profile");

        let result = pack
            .dispatch("brain.profiles", json!({}), &registry, &token)
            .await
            .expect("profiles list");

        let profiles = result["profiles"].as_array().expect("profiles array");
        let ids: Vec<&str> = profiles.iter().filter_map(|p| p["id"].as_str()).collect();

        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(
            ids, sorted,
            "brain.profiles must return profiles sorted by id"
        );
    }
}

// ── CRIT-1 regression tests: seed-prior ESS gate + no panic/poison path ──────

#[tokio::test]
async fn crit1_create_profile_rejects_seed_priors_exceeding_ess_cap() {
    // alpha + beta = 1000 > DEFAULT_ESS_CAP (100.0) → must be rejected.
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    let err = pack
        .dispatch(
            "brain.create_profile",
            json!({
                "name": "bad-ess-profile",
                "consumer_kind": "search",
                "seed_priors": {
                    "section_posteriors": {
                        "operational_guidance": {"alpha": 500.0, "beta": 500.0}
                    }
                }
            }),
            &registry,
            &token,
        )
        .await
        .unwrap_err();

    if let RuntimeError::InvalidInput(msg) = &err {
        assert!(
            msg.contains("ESS") || msg.contains("alpha+beta") || msg.contains("exceeds"),
            "error must mention the ESS constraint; got: {msg}"
        );
    } else {
        panic!("expected InvalidInput, got {err:?}");
    }
}

#[tokio::test]
async fn crit1_create_profile_accepts_seed_priors_within_ess_cap() {
    // alpha + beta = 7.5 <= DEFAULT_ESS_CAP (100.0) → must succeed.
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    let result = pack
        .dispatch(
            "brain.create_profile",
            json!({
                "name": "ok-ess-profile",
                "consumer_kind": "search",
                "seed_priors": {
                    "section_posteriors": {
                        "operational_guidance": {"alpha": 6.0, "beta": 1.5}
                    }
                }
            }),
            &registry,
            &token,
        )
        .await
        .expect("create with valid seed priors must succeed");

    assert_eq!(result["created"], json!(true));
}

// ── Concurrency / publication-atomicity regression tests ──────────────────────
//
// These tests were added to guard against the TOCTOU race in ensure_loaded where
// active_namespace, *state, and loaded_namespaces were updated in three separate
// critical sections.  After the fix all three are updated atomically while the
// tracker lock is held, so a concurrent dispatch can never observe active=true
// with stale state.

/// After ensure_loaded returns, the tracker and shared state must be in a
/// consistent three-way view: active_namespace == namespace, loaded_namespaces
/// contains namespace, and the shared BrainState is the one for that namespace.
///
/// We test this by:
///   1. Loading namespace A and writing a binding (observable state mutation).
///   2. Loading namespace B — saves A's state, loads fresh B state.
///   3. Switching back to namespace A — restores the saved state.
///   4. After each ensure_loaded the tracker fields must be consistent.
#[tokio::test]
async fn ensure_loaded_publication_is_atomic() {
    use core::convert::TryFrom;
    use khive_runtime::Namespace;

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let pack = BrainPack::new(rt.clone());
    let registry = empty_registry();

    let ns_a = Namespace::try_from("atomic-ns-a").expect("namespace a");
    let ns_b = Namespace::try_from("atomic-ns-b").expect("namespace b");
    let token_a = rt.authorize(ns_a).expect("token a");
    let token_b = rt.authorize(ns_b).expect("token b");

    // Load namespace A by dispatching a read verb.
    pack.ensure_loaded(&token_a)
        .await
        .expect("ensure_loaded ns-a");

    // Invariant 1: after ensure_loaded, tracker must be fully consistent for A.
    {
        let t = pack.persistence.lock().unwrap();
        assert_eq!(
            t.active_namespace.as_deref(),
            Some("atomic-ns-a"),
            "active_namespace must equal requested namespace immediately after ensure_loaded"
        );
        assert!(
            t.loaded_namespaces.contains_key("atomic-ns-a"),
            "loaded_namespaces must contain the namespace immediately after ensure_loaded"
        );
    }
    // The shared state must also reflect A (profile list should be visible).
    {
        let s = pack.state.lock().unwrap();
        assert!(
            !s.profiles.is_empty(),
            "shared state must contain at least the built-in profile after loading ns-a"
        );
    }

    // Write something observable into A: create a binding.
    pack.dispatch(
        "brain.bind",
        json!({
            "profile_id": "balanced-recall-v1",
            "actor": "actor-a",
            "namespace": "atomic-ns-a",
            "consumer_kind": "recall",
        }),
        &registry,
        &token_a,
    )
    .await
    .expect("bind in namespace A");

    // Load namespace B.
    pack.ensure_loaded(&token_b)
        .await
        .expect("ensure_loaded ns-b");

    // Invariant 2: tracker must be fully consistent for B.
    {
        let t = pack.persistence.lock().unwrap();
        assert_eq!(
            t.active_namespace.as_deref(),
            Some("atomic-ns-b"),
            "active_namespace must equal namespace B after switching to B"
        );
        assert!(
            t.loaded_namespaces.contains_key("atomic-ns-b"),
            "loaded_namespaces must contain ns-b after ensure_loaded"
        );
        assert!(
            t.loaded_namespaces.contains_key("atomic-ns-a"),
            "loaded_namespaces must still contain ns-a (saved, not evicted)"
        );
    }
    // The shared state must reflect B: B has no bindings (was never written to).
    let bindings_b = pack
        .dispatch("brain.bindings", json!({}), &registry, &token_b)
        .await
        .expect("bindings in B");
    assert_eq!(
        bindings_b["count"],
        json!(0u64),
        "namespace B must see 0 bindings; the binding created in A must not bleed through"
    );

    // Switch back to namespace A — the save-restore path.
    pack.ensure_loaded(&token_a)
        .await
        .expect("ensure_loaded ns-a again");

    // Invariant 3: tracker consistent for A again.
    {
        let t = pack.persistence.lock().unwrap();
        assert_eq!(
            t.active_namespace.as_deref(),
            Some("atomic-ns-a"),
            "active_namespace must be restored to A"
        );
    }
    // The binding we created in A must still be present (save-restore preserved state).
    let bindings_a = pack
        .dispatch("brain.bindings", json!({}), &registry, &token_a)
        .await
        .expect("bindings in A after restore");
    assert_eq!(
        bindings_a["count"],
        json!(1u64),
        "binding created in namespace A must survive the save-restore round-trip"
    );
}

/// Concurrent dispatches to the same namespace must not observe active=true
/// with stale state.  We spawn N tasks that all call ensure_loaded for the
/// same namespace concurrently; after they all complete the state must be
/// consistent (tracker and shared state agree, no panic, no data loss).
#[tokio::test]
async fn ensure_loaded_concurrent_same_namespace_is_safe() {
    use core::convert::TryFrom;
    use khive_runtime::Namespace;
    use std::sync::Arc;

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let pack = Arc::new(BrainPack::new(rt.clone()));
    let registry = empty_registry();

    let ns = Namespace::try_from("conc-ns").expect("conc namespace");
    let token = rt.authorize(ns).expect("conc token");
    let token = Arc::new(token);

    // Spawn 8 concurrent ensure_loaded calls for the same namespace.
    let mut handles = Vec::new();
    for _ in 0..8 {
        let pack2 = Arc::clone(&pack);
        let tok2 = Arc::clone(&token);
        handles.push(tokio::spawn(
            async move { pack2.ensure_loaded(&tok2).await },
        ));
    }
    for h in handles {
        h.await
            .expect("task did not panic")
            .expect("ensure_loaded must not error");
    }

    // After all concurrent loads complete, tracker and state must agree.
    {
        let t = pack.persistence.lock().unwrap();
        assert_eq!(
            t.active_namespace.as_deref(),
            Some("conc-ns"),
            "active_namespace must be conc-ns after concurrent loads"
        );
        assert!(
            t.loaded_namespaces.contains_key("conc-ns"),
            "loaded_namespaces must contain conc-ns"
        );
    }
    {
        let s = pack.state.lock().unwrap();
        assert!(
            !s.profiles.is_empty(),
            "shared state must be non-empty (built-in profile present)"
        );
    }

    // A dispatch against the namespace must succeed without panicking.
    let result = pack
        .dispatch("brain.profiles", json!({}), &registry, &token)
        .await
        .expect("brain.profiles after concurrent ensure_loaded");
    assert!(
        result["count"].as_u64().unwrap_or(0) >= 1,
        "at least the built-in profile must be present"
    );
}

/// Cross-namespace concurrent loads must not cause one namespace to save the
/// wrong state under saved_states.  We alternate between two namespaces while
/// mutating each, then verify each namespace sees only its own state.
#[tokio::test]
async fn ensure_loaded_cross_namespace_concurrent_does_not_corrupt_saved_states() {
    use core::convert::TryFrom;
    use khive_runtime::Namespace;

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let pack = std::sync::Arc::new(BrainPack::new(rt.clone()));
    let registry = empty_registry();

    let ns_x = Namespace::try_from("xns-x").expect("x");
    let ns_y = Namespace::try_from("xns-y").expect("y");
    let token_x = rt.authorize(ns_x).expect("token x");
    let token_y = rt.authorize(ns_y).expect("token y");
    let token_x = std::sync::Arc::new(token_x);
    let token_y = std::sync::Arc::new(token_y);

    // Interleave loads: X then Y then X then Y.
    pack.ensure_loaded(&token_x).await.expect("load x");
    pack.dispatch(
        "brain.bind",
        json!({"profile_id": "balanced-recall-v1", "actor": "x-actor", "namespace": "xns-x", "consumer_kind": "recall"}),
        &registry,
        &token_x,
    )
    .await
    .expect("bind x");

    pack.ensure_loaded(&token_y).await.expect("load y");
    // Y has no bindings yet; add one so it has observable state too.
    pack.dispatch(
        "brain.bind",
        json!({"profile_id": "balanced-recall-v1", "actor": "y-actor", "namespace": "xns-y", "consumer_kind": "recall"}),
        &registry,
        &token_y,
    )
    .await
    .expect("bind y");

    // Switch back to X and verify its binding survived.
    pack.ensure_loaded(&token_x).await.expect("reload x");
    let bx = pack
        .dispatch("brain.bindings", json!({}), &registry, &token_x)
        .await
        .expect("bindings x");
    assert_eq!(
        bx["count"],
        json!(1u64),
        "namespace X must still have exactly 1 binding after cross-namespace interleave"
    );

    // Switch to Y and verify its binding survived.
    pack.ensure_loaded(&token_y).await.expect("reload y");
    let by = pack
        .dispatch("brain.bindings", json!({}), &registry, &token_y)
        .await
        .expect("bindings y");
    assert_eq!(
        by["count"],
        json!(1u64),
        "namespace Y must still have exactly 1 binding after cross-namespace interleave"
    );
}

/// Deterministic interleaving test for the concurrent cold-load race.
///
/// Interleaving manufactured by the test-only `POST_LOAD_HOOK` in persist.rs:
///
///   1. Loader B is spawned; it calls `ensure_loaded` for "race-ns", completes
///      the async DB scan, then PAUSES at the hook before acquiring the final
///      tracker lock (it signals "reached" on a oneshot and awaits "proceed").
///   2. While B is paused, the test task runs Loader A to completion (no hook
///      active for A — hook was already consumed by B's `.take()`).  A publishes
///      "race-ns" as active.
///   3. The test task mutates state via `brain.bind` (adds one binding to A's
///      namespace).  Binding count is now 1.
///   4. The test task sends "proceed" to B.  B resumes, enters the final
///      tracker block, and:
///        • OLD code: sees `current_ns = Some("race-ns")`, takes the
///          `swap_namespace` path, and B's stale cold-loaded `brain_state`
///          (binding count 0) overrides the live state.  Final binding
///          count = 0.  TEST FAILS.
///        • FIXED code: re-checks `is_active("race-ns")` — true — and
///          returns early without touching `*state`.  Final binding
///          count = 1.  TEST PASSES.
///
/// FAIL-before / PASS-after evidence is produced by running:
///   cargo test -p khive-pack-brain -- concurrent_cold_load_does_not_clobber_live_state
/// against the reverted commit and against the fixed commit.
#[tokio::test]
async fn concurrent_cold_load_does_not_clobber_live_state() {
    use core::convert::TryFrom;
    use khive_runtime::Namespace;
    use std::sync::Arc;
    use tokio::sync::oneshot;

    // Always clean up the hook, even on panic.
    struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            persist::clear_post_load_hook();
        }
    }
    let _guard = HookGuard;

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let pack = Arc::new(BrainPack::new(rt.clone()));
    let registry = empty_registry();

    let ns = Namespace::try_from("race-ns-det").expect("race namespace");
    let token = Arc::new(rt.authorize(ns).expect("race token"));

    // ── Step 1: register the hook for Loader B ────────────────────────────────
    // B will fire the hook once it has finished the cold DB load.
    let (reached_tx, reached_rx) = oneshot::channel::<()>();
    let (proceed_tx, proceed_rx) = oneshot::channel::<()>();
    persist::set_post_load_hook(persist::LoadHook {
        reached_tx,
        proceed_rx,
    });

    // ── Step 2: spawn Loader B ────────────────────────────────────────────────
    // B will pause at the hook (awaiting proceed_rx) once the hook fires.
    let pack_b = Arc::clone(&pack);
    let token_b = Arc::clone(&token);
    let b_handle = tokio::spawn(async move { pack_b.ensure_loaded(&token_b).await });

    // Wait for B to signal it has reached the hook (DB load done, not yet in
    // the final tracker block).
    reached_rx.await.expect("loader B must signal reached");

    // ── Step 3: run Loader A to completion ────────────────────────────────────
    // The hook was consumed by B's `.take()`, so A passes straight through.
    // A publishes "race-ns-det" as active and returns.
    pack.ensure_loaded(&token)
        .await
        .expect("loader A: ensure_loaded");

    // ── Step 4: mutate state under A's namespace ──────────────────────────────
    pack.dispatch(
        "brain.bind",
        json!({
            "profile_id": "balanced-recall-v1",
            "actor": "racer",
            "namespace": "race-ns-det",
            "consumer_kind": "recall",
        }),
        &registry,
        &token,
    )
    .await
    .expect("brain.bind after A loaded");

    // Binding count is now 1.
    let before = pack
        .dispatch("brain.bindings", json!({}), &registry, &token)
        .await
        .expect("bindings before B resumes");
    assert_eq!(
        before["count"],
        json!(1u64),
        "binding must exist before B resumes"
    );

    // ── Step 5: release Loader B ──────────────────────────────────────────────
    // B now enters the final tracker block.
    // OLD code: B overwrites *state with its stale cold-loaded copy → count=0 → FAIL.
    // FIXED code: B re-checks is_active("race-ns-det") → true → returns early → count=1 → PASS.
    proceed_tx.send(()).expect("send proceed to B");
    b_handle
        .await
        .expect("loader B task must not panic")
        .expect("loader B ensure_loaded must not error");

    // ── Step 6: assert the mutation survived ─────────────────────────────────
    let after = pack
        .dispatch("brain.bindings", json!({}), &registry, &token)
        .await
        .expect("bindings after B resumes");
    assert_eq!(
        after["count"],
        json!(1u64),
        "concurrent cold-load race: Loader B must not clobber the binding \
         created by Loader A (old code would return 0 here)"
    );
}
