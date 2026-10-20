// ── Regression tests ──────────────────────────────────────────────────────

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
    create_active_lifecycle_profile(&pack, &registry, &token).await;

    // Archive the custom profile.
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

    let err = pack
        .dispatch(
            "brain.bind",
            json!({"profile_id": LIFECYCLE_PROFILE_ID, "consumer_kind": "recall"}),
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
    let resolved = pack
        .dispatch(
            "brain.resolve",
            json!({"consumer_kind": "recall"}),
            &registry,
            &token,
        )
        .await
        .expect("a registered consumer without a non-archived profile uses tier 3");
    assert_eq!(
        resolved,
        json!({
            "resolved_profile_id": null,
            "lifecycle": null,
            "requested_consumer_kind": "recall",
            "matched_consumer_kind": null,
            "matched_binding": false,
        })
    );
}
