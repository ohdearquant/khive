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
    create_active_lifecycle_profile(&pack, &registry, &token).await;

    // Deactivate the custom active profile
    let result = pack
        .dispatch(
            "brain.deactivate",
            json!({"profile_id": LIFECYCLE_PROFILE_ID}),
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
            json!({"id": LIFECYCLE_PROFILE_ID}),
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
            json!({"profile_id": LIFECYCLE_PROFILE_ID}),
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
    create_active_lifecycle_profile(&pack, &registry, &token).await;

    // Lifecycle DAG requires active → inactive before archiving.
    pack.dispatch(
        "brain.deactivate",
        json!({"profile_id": LIFECYCLE_PROFILE_ID}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    let result = pack
        .dispatch(
            "brain.archive",
            json!({"profile_id": LIFECYCLE_PROFILE_ID}),
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
    let (pack, rt) = make_pack_with_actor("agent-x");
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
    assert_eq!(result["removed"], json!(1u64));
    assert_eq!(result["unbound"], json!(1u64));
}

#[tokio::test]
async fn dispatch_unbind_reports_zero_when_selector_matches_nothing() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    let result = pack
        .dispatch(
            "brain.unbind",
            json!({"profile_id": "well-formed-but-unbound"}),
            &registry,
            &token,
        )
        .await
        .unwrap();

    assert_eq!(result["removed"], json!(0u64));
    assert_eq!(result["unbound"], json!(0u64));
}
