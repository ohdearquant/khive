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
