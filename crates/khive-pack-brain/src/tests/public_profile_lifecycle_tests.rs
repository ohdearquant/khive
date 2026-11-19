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
    create_active_lifecycle_profile(&pack, &registry, &token).await;

    // Deactivate first (active → inactive), then archive (inactive → archived)
    pack.dispatch(
        "brain.deactivate",
        json!({"profile_id": LIFECYCLE_PROFILE_ID}),
        &registry,
        &token,
    )
    .await
    .unwrap();
    pack.dispatch(
        "brain.archive",
        json!({"profile_id": LIFECYCLE_PROFILE_ID}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    // Attempt to activate an archived profile — must fail
    let err = pack
        .dispatch(
            "brain.activate",
            json!({"profile_id": LIFECYCLE_PROFILE_ID}),
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
    create_active_lifecycle_profile(&pack, &registry, &token).await;

    // Get to archived via active → inactive → archived
    pack.dispatch(
        "brain.deactivate",
        json!({"profile_id": LIFECYCLE_PROFILE_ID}),
        &registry,
        &token,
    )
    .await
    .unwrap();
    pack.dispatch(
        "brain.archive",
        json!({"profile_id": LIFECYCLE_PROFILE_ID}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    // Attempt deactivate on archived profile — must fail
    let err = pack
        .dispatch(
            "brain.deactivate",
            json!({"profile_id": LIFECYCLE_PROFILE_ID}),
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
    create_active_lifecycle_profile(&pack, &registry, &token).await;

    // Profile starts active — direct archive must fail (must go through inactive)
    let err = pack
        .dispatch(
            "brain.archive",
            json!({"profile_id": LIFECYCLE_PROFILE_ID}),
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
    create_active_lifecycle_profile(&pack, &registry, &token).await;

    // active → inactive → archived: legal path
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
    assert_eq!(
        result["lifecycle"],
        json!("archived"),
        "B-C1: inactive→archived must succeed"
    );
}
