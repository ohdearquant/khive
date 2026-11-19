// brain.create_profile rejects empty and wildcard consumer_kind.
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
        "empty consumer_kind must return InvalidInput, got {err:?}"
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
            "wildcard consumer_kind rejection must explain the issue; got: {msg}"
        );
    } else {
        panic!("wildcard consumer_kind must return InvalidInput, got {err:?}");
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
        "whitespace consumer_kind must return InvalidInput, got {err:?}"
    );
}

// brain.bindings AND-semantics pinned with ≥3 bindings and combined filters.
#[tokio::test]
async fn r2_bindings_and_semantics_multi_filter() {
    let (pack, rt) = make_pack_with_read_scope(Some("agent-A"), &["agent-B"], &[]);
    let registry = empty_registry();
    let token = rt
        .authorize_with_visibility(Namespace::local(), rt.visible_namespaces().to_vec())
        .unwrap();

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

// User-created profile has real posterior state that reset mutates.
// Emit feedback first to move posteriors, then assert all three
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
        "feedback must have moved salience alpha above prior 2.0; got {salience_alpha_before}"
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
        "reset must increment exploration_epoch on user-created profile; before={epoch_before} after={epoch_after}"
    );

    // All three posteriors must return exactly to priors:
    //   relevance  = Beta(7, 3)
    //   salience   = Beta(2, 8)
    //   temporal   = Beta(1, 9)
    let snap = &after["state_snapshot"];
    assert!(
        !snap.is_null(),
        "state_snapshot must be non-null after reset"
    );

    let rel_alpha = snap["relevance"]["alpha"]
        .as_f64()
        .expect("relevance.alpha()");
    let rel_beta = snap["relevance"]["beta"]
        .as_f64()
        .expect("relevance.beta()");
    assert!(
        (rel_alpha - 7.0).abs() < 1e-9 && (rel_beta - 3.0).abs() < 1e-9,
        "relevance must be Beta(7,3) after reset; got ({rel_alpha},{rel_beta})"
    );

    let sal_alpha = snap["salience"]["alpha"]
        .as_f64()
        .expect("salience.alpha()");
    let sal_beta = snap["salience"]["beta"].as_f64().expect("salience.beta()");
    assert!(
        (sal_alpha - 2.0).abs() < 1e-9 && (sal_beta - 8.0).abs() < 1e-9,
        "salience must be Beta(2,8) after reset; got ({sal_alpha},{sal_beta})"
    );

    let tmp_alpha = snap["temporal"]["alpha"]
        .as_f64()
        .expect("temporal.alpha()");
    let tmp_beta = snap["temporal"]["beta"].as_f64().expect("temporal.beta()");
    assert!(
        (tmp_alpha - 1.0).abs() < 1e-9 && (tmp_beta - 9.0).abs() < 1e-9,
        "temporal must be Beta(1,9) after reset; got ({tmp_alpha},{tmp_beta})"
    );
}
