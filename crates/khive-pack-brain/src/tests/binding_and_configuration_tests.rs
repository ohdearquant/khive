// Unbind combines supplied filters with AND semantics and removes only
// the binding that satisfies every criterion.
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
