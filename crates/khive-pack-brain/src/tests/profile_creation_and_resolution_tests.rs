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
    let (pack, rt) = make_pack_with_actor("agent-a");
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

#[tokio::test]
async fn bind_rejects_unregistered_consumer_kind_with_valid_list() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    let err = pack
        .dispatch(
            "brain.bind",
            json!({
                "profile_id": "balanced-recall-v1",
                "consumer_kind": "never_requested",
            }),
            &registry,
            &token,
        )
        .await
        .expect_err("an unregistered consumer kind must be rejected");

    let RuntimeError::InvalidInput(message) = err else {
        panic!("expected InvalidInput, got {err:?}");
    };
    assert!(message.contains("never_requested"), "{message}");
    for valid in ["*", "knowledge_compose", "recall", "search"] {
        assert!(
            message.contains(valid),
            "valid consumer kind {valid:?} missing from error: {message}"
        );
    }

    let bindings = pack
        .dispatch("brain.bindings", json!({}), &registry, &token)
        .await
        .expect("bindings remain readable after rejected bind");
    assert_eq!(bindings["count"], json!(0u64));
}

// H2: brain.bindings supports filtering.
#[tokio::test]
async fn w4_h2_bindings_filtered() {
    let (pack, rt) = make_pack_with_actor("agent-1");
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

// Archived high-priority binding + live lower-priority wildcard → live wins.
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
        "archived high-priority binding must not suppress the live wildcard binding"
    );
}
