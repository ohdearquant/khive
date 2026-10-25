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
        h.params.iter().any(|p| p.name == "id" && p.required),
        "brain.feedback must have required id param"
    );
    assert!(
        h.params.iter().all(|p| p.name != "target_id"),
        "brain.feedback must advertise only the canonical id parameter"
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

/// ADR-084 Rule 3 (help-schema fidelity): scorer_run_id/serve_ledger_id are
/// accepted params (ADR-081 §6) that must be declared in the help=true
/// surface, not just parsed internally — a hidden accepted param is
/// exactly the drift Rule 3 exists to catch.
#[test]
fn brain_feedback_declares_adr081_dedup_key_params() {
    let h = find_handler("brain.feedback");
    let scorer_run_id = h
        .params
        .iter()
        .find(|p| p.name == "scorer_run_id")
        .unwrap_or_else(|| panic!("brain.feedback must declare scorer_run_id"));
    assert!(
            !scorer_run_id.required,
            "scorer_run_id is optional (together-or-rejected with serve_ledger_id, not always required)"
        );
    assert!(
        scorer_run_id.description.contains("dedup")
            && scorer_run_id.description.contains("together")
            && scorer_run_id.description.contains("serve_ledger_id"),
        "scorer_run_id description must document the dedup + together-or-rejected pairing: {:?}",
        scorer_run_id.description
    );

    let serve_ledger_id = h
        .params
        .iter()
        .find(|p| p.name == "serve_ledger_id")
        .unwrap_or_else(|| panic!("brain.feedback must declare serve_ledger_id"));
    assert!(
        !serve_ledger_id.required,
        "serve_ledger_id is optional (together-or-rejected with scorer_run_id)"
    );
    assert!(
        serve_ledger_id.description.contains("dedup")
            && serve_ledger_id.description.contains("together")
            && serve_ledger_id.description.contains("scorer_run_id"),
        "serve_ledger_id description must document the dedup + together-or-rejected pairing: {:?}",
        serve_ledger_id.description
    );
}

#[test]
fn brain_auto_feedback_handler_is_declared() {
    let h = find_handler("brain.auto_feedback");
    assert!(
        h.params.iter().any(|p| p.name == "query" && p.required),
        "brain.auto_feedback must have required query param"
    );
    let results = h
        .params
        .iter()
        .find(|p| p.name == "results")
        .unwrap_or_else(|| panic!("brain.auto_feedback must declare results"));
    assert!(results.required, "results must be required");
    assert!(
        results.description.contains("id or full_id")
            && results.description.contains("exactly one result object")
            && results.description.contains("count once")
            && results.description.contains("full_id must be a full UUID"),
        "results help must describe alias uniqueness and canonical identity: {:?}",
        results.description
    );
    let target_id = h
        .params
        .iter()
        .find(|p| p.name == "target_id")
        .unwrap_or_else(|| panic!("brain.auto_feedback must declare target_id"));
    assert!(!target_id.required, "target_id is conditional on signal");
    assert_eq!(
        target_id.param_type, "string",
        "target_id accepts either a full UUID or the compact id returned by recall"
    );
    assert!(
        target_id.description.contains("compact id")
            && target_id.description.contains("full_id")
            && target_id.description.contains("Required when signal")
            && target_id.description.contains("exactly one result object")
            && target_id.description.contains("count once")
            && target_id.description.contains("must be a full UUID"),
        "target_id help must describe conditional uniqueness: {:?}",
        target_id.description
    );
    let signal = h
        .params
        .iter()
        .find(|p| p.name == "signal")
        .unwrap_or_else(|| panic!("brain.auto_feedback must declare signal"));
    assert!(!signal.required);
    assert!(
        signal.description.contains("Omission means abstain"),
        "signal help must not imply an automatic positive: {:?}",
        signal.description
    );
    let namespace = h
        .params
        .iter()
        .find(|p| p.name == "namespace")
        .unwrap_or_else(|| panic!("brain.auto_feedback must declare namespace"));
    assert!(!namespace.required);
    assert!(
        namespace.description.contains("Exact") && namespace.description.contains("posterior"),
        "namespace help must document exact posterior isolation: {:?}",
        namespace.description
    );
}

/// Mirrors `brain_feedback_declares_adr081_dedup_key_params`: auto_feedback
/// forwards scorer_run_id/serve_ledger_id verbatim to brain.feedback, so
/// its own help surface must document them too (ADR-084 Rule 3).
#[test]
fn brain_auto_feedback_declares_adr081_dedup_key_params() {
    let h = find_handler("brain.auto_feedback");
    let scorer_run_id = h
        .params
        .iter()
        .find(|p| p.name == "scorer_run_id")
        .unwrap_or_else(|| panic!("brain.auto_feedback must declare scorer_run_id"));
    assert!(!scorer_run_id.required);
    assert!(
        scorer_run_id.description.contains("serve_ledger_id"),
        "scorer_run_id description must document the together-or-rejected pairing: {:?}",
        scorer_run_id.description
    );

    let serve_ledger_id = h
        .params
        .iter()
        .find(|p| p.name == "serve_ledger_id")
        .unwrap_or_else(|| panic!("brain.auto_feedback must declare serve_ledger_id"));
    assert!(!serve_ledger_id.required);
    assert!(
        serve_ledger_id.description.contains("scorer_run_id"),
        "serve_ledger_id description must document the together-or-rejected pairing: {:?}",
        serve_ledger_id.description
    );
}

include!("help_tests/profile_and_binding_parameters.rs");

include!("help_tests/additional_parameters.rs");

// ── Regression: schema-aware namespace strip ──────────
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
    let (_, rt) = make_pack_with_actor("alice");
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("alice".to_string()));
    builder.register(KgPack::new(rt.clone()));
    builder.register(BrainPack::new(rt.clone()));
    let registry = builder.build().expect("kg+brain registry builds");
    (registry, rt)
}

/// #1505: an explicit auto-feedback namespace is both the event stamp and
/// the posterior-state owner. The default/live namespace must remain
/// byte-for-byte at its pre-call event count.
#[tokio::test]
async fn auto_feedback_namespace_does_not_train_default_posteriors() {
    let (registry, rt) = make_brain_registry();
    let local_token = rt.authorize(Namespace::local()).expect("local token");
    let target = create_test_entity(&rt, &local_token).await;

    let profile_total = |value: &serde_json::Value| {
        value["total_events"]
            .as_u64()
            .expect("profile total_events")
    };
    let local_before = registry
        .dispatch("brain.profile", json!({"profile_id": "balanced-recall-v1"}))
        .await
        .expect("read local profile before feedback");
    let arm_before = registry
        .dispatch(
            "brain.profile",
            json!({
                "profile_id": "balanced-recall-v1",
                "namespace": "bench-arm-a",
            }),
        )
        .await
        .expect("read arm profile before feedback");

    let feedback = registry
        .dispatch(
            "brain.auto_feedback",
            json!({
                "query": "isolated recall measurement",
                "results": [{"id": target}],
                "target_id": target,
                "signal": "implicit_positive",
                "namespace": "bench-arm-a",
            }),
        )
        .await
        .expect("namespace-scoped auto_feedback");
    assert_eq!(feedback["emitted"], json!(true), "got: {feedback}");

    let arm_after = registry
        .dispatch(
            "brain.profile",
            json!({
                "profile_id": "balanced-recall-v1",
                "namespace": "bench-arm-a",
            }),
        )
        .await
        .expect("read arm profile after feedback");
    let local_after = registry
        .dispatch("brain.profile", json!({"profile_id": "balanced-recall-v1"}))
        .await
        .expect("read local profile after feedback");

    assert_eq!(
        profile_total(&arm_after),
        profile_total(&arm_before) + 1,
        "the explicit arm must receive exactly one feedback fold"
    );
    assert_eq!(
        profile_total(&local_after),
        profile_total(&local_before),
        "arm feedback must not train the default/live posterior"
    );

    let arm_token = rt
        .authorize(Namespace::parse("bench-arm-a").expect("arm namespace"))
        .expect("arm token");
    let event_id = uuid::Uuid::parse_str(
        feedback["event_id"]
            .as_str()
            .expect("feedback response event_id"),
    )
    .expect("event UUID");
    let event = rt
        .events(&arm_token)
        .expect("event store")
        .get_event(event_id)
        .await
        .expect("event lookup")
        .expect("feedback event exists");
    assert_eq!(event.namespace, "bench-arm-a");
}

/// brain.bind via VerbRegistry must store the caller-supplied namespace,
/// not default to "*".  Regression for the blanket-strip bug.
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
/// match the binding stored by brain.bind.  Regression for the binding match bug.
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
/// remove only the matching binding.  Regression for the binding removal bug.
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
