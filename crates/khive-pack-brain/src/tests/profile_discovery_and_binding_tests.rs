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

#[tokio::test]
async fn dispatch_bind_team_catalog_resolves_only_in_team_catalog() {
    use core::convert::TryFrom;

    let (pack, rt) = make_pack_with_actor("catalog-match");
    let registry = empty_registry();
    let local = rt.authorize(Namespace::local()).unwrap();
    let team = rt.authorize(Namespace::try_from("team-a").unwrap()).unwrap();
    let profile_id = "team-catalog-profile";
    let created = pack
        .dispatch(
            "brain.create_profile",
            json!({"name": profile_id, "consumer_kind": "recall"}),
            &registry,
            &team,
        )
        .await
        .unwrap();
    assert_eq!(created["created"], json!(true));
    assert_eq!(created["profile_id"], json!(profile_id));
    pack.dispatch("brain.activate", json!({"profile_id": profile_id}), &registry, &team)
        .await
        .unwrap();
    let profiles = pack.dispatch("brain.profiles", json!({}), &registry, &team).await.unwrap();
    assert_eq!(profiles["namespace"], json!("team-a"));
    assert!(profiles["profiles"].as_array().unwrap().iter().any(|p| p["id"] == json!(profile_id)));
    let profile = pack
        .dispatch("brain.profile", json!({"profile_id": profile_id}), &registry, &team)
        .await
        .unwrap();
    assert_eq!(profile["namespace"], json!("team-a"));
    assert_eq!(profile["id"], json!(profile_id));
    assert_eq!(profile["lifecycle"], json!("active"));
    let local_profiles = pack.dispatch("brain.profiles", json!({}), &registry, &local).await.unwrap();
    assert_eq!(local_profiles["namespace"], json!("local"));
    assert!(!local_profiles["profiles"].as_array().unwrap().iter().any(|p| p["id"] == json!(profile_id)));
    let local_profile = pack
        .dispatch("brain.profile", json!({"profile_id": "balanced-recall-v1"}), &registry, &local)
        .await
        .unwrap();
    assert_eq!(local_profile["namespace"], json!("local"));
    assert_eq!(local_profile["id"], json!("balanced-recall-v1"));

    let bound = pack
        .dispatch(
            "brain.bind",
            json!({"profile_id": profile_id, "actor": "catalog-match", "namespace": "team-a", "consumer_kind": "recall"}),
            &registry,
            &team,
        )
        .await
        .unwrap();
    assert_eq!(bound["bound"], json!(true));
    assert_eq!(bound["namespace"], json!("team-a"));
    let bindings = pack
        .dispatch("brain.bindings", json!({"actor": "catalog-match", "namespace": "team-a"}), &registry, &team)
        .await
        .unwrap();
    assert_eq!(bindings["namespace"], json!("team-a"));
    assert_eq!(bindings["count"], json!(1));
    assert_eq!(bindings["bindings"][0]["profile_id"], json!(profile_id));
    assert_eq!(bindings["bindings"][0]["namespace"], json!("team-a"));
    let resolved = pack
        .dispatch(
            "brain.resolve",
            json!({"actor": "catalog-match", "namespace": "team-a", "consumer_kind": "recall"}),
            &registry,
            &team,
        )
        .await
        .unwrap();
    assert_eq!(resolved["resolved_profile_id"], json!(profile_id));
    assert_eq!(resolved["matched_binding"], json!(true));
    let local_resolved = pack
        .dispatch(
            "brain.resolve",
            json!({"actor": "catalog-match", "namespace": "local", "consumer_kind": "recall"}),
            &registry,
            &local,
        )
        .await
        .unwrap();
    assert_eq!(local_resolved["matched_binding"], json!(false));
    assert_ne!(local_resolved["resolved_profile_id"], json!(profile_id));
    let local_bindings = pack
        .dispatch("brain.bindings", json!({"actor": "catalog-match", "namespace": "local"}), &registry, &local)
        .await
        .unwrap();
    assert_eq!(local_bindings["namespace"], json!("local"));
    assert_eq!(local_bindings["count"], json!(0));
}

#[tokio::test]
async fn dispatch_bind_missing_profile_names_searched_catalog() {
    use core::convert::TryFrom;

    let (pack, rt) = make_pack_with_actor("catalog-refusal");
    let registry = empty_registry();
    let local = rt.authorize(Namespace::local()).unwrap();
    let team = rt.authorize(Namespace::try_from("team-a").unwrap()).unwrap();
    let profile_id = "local-only-profile";
    let created = pack
        .dispatch(
            "brain.create_profile",
            json!({"name": profile_id, "consumer_kind": "recall"}),
            &registry,
            &local,
        )
        .await
        .unwrap();
    assert_eq!(created["created"], json!(true));
    assert_eq!(created["profile_id"], json!(profile_id));
    let local_profiles = pack.dispatch("brain.profiles", json!({}), &registry, &local).await.unwrap();
    assert_eq!(local_profiles["namespace"], json!("local"));
    assert!(local_profiles["profiles"].as_array().unwrap().iter().any(|p| p["id"] == json!(profile_id)));
    let local_profile = pack
        .dispatch("brain.profile", json!({"profile_id": profile_id}), &registry, &local)
        .await
        .unwrap();
    assert_eq!(local_profile["namespace"], json!("local"));
    assert_eq!(local_profile["id"], json!(profile_id));
    let team_profiles = pack.dispatch("brain.profiles", json!({}), &registry, &team).await.unwrap();
    assert_eq!(team_profiles["namespace"], json!("team-a"));
    assert!(!team_profiles["profiles"].as_array().unwrap().iter().any(|p| p["id"] == json!(profile_id)));
    let team_profile = pack
        .dispatch("brain.profile", json!({"profile_id": "balanced-recall-v1"}), &registry, &team)
        .await
        .unwrap();
    assert_eq!(team_profile["namespace"], json!("team-a"));
    assert_eq!(team_profile["id"], json!("balanced-recall-v1"));
    let before = pack
        .dispatch("brain.bindings", json!({"actor": "catalog-refusal", "namespace": "team-a"}), &registry, &team)
        .await
        .unwrap();
    assert_eq!(before["namespace"], json!("team-a"));
    assert_eq!(before["count"], json!(0));

    let error = pack
        .dispatch(
            "brain.bind",
            json!({"profile_id": profile_id, "actor": "catalog-refusal", "namespace": "team-a", "consumer_kind": "recall"}),
            &registry,
            &team,
        )
        .await
        .unwrap_err();
    let RuntimeError::NotFound(message) = error else {
        panic!("bind must refuse a profile absent from the selected catalog with NotFound");
    };
    assert_eq!(
        message,
        r#"profile "local-only-profile" not found in namespace "team-a"; a profile created with namespace=X is bound with namespace=X"#,
        "bind_not_found_names_searched_catalog"
    );
    let after = pack
        .dispatch("brain.bindings", json!({"actor": "catalog-refusal", "namespace": "team-a"}), &registry, &team)
        .await
        .unwrap();
    assert_eq!(after["namespace"], json!("team-a"));
    assert_eq!(after["count"], json!(0));
}
