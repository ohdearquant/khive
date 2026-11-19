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
async fn brain_reset_help_and_dispatch_accept_inactive_profile() {
    let help = crate::handlers::BRAIN_HANDLERS
        .iter()
        .find(|handler| handler.name == "brain.reset")
        .expect("brain.reset handler");
    let profile_help = help
        .params
        .iter()
        .find(|param| param.name == "profile_id")
        .expect("profile_id parameter");
    assert!(profile_help.description.contains("non-archived"));

    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let created = pack
        .dispatch(
            "brain.create_profile",
            json!({"name": "inactive-reset-v1", "consumer_kind": "recall"}),
            &registry,
            &token,
        )
        .await
        .expect("create inactive profile");
    assert_eq!(created["lifecycle"], "inactive");
    let reset = pack
        .dispatch(
            "brain.reset",
            json!({"profile_id": "inactive-reset-v1"}),
            &registry,
            &token,
        )
        .await
        .expect("inactive profile is eligible for reset");
    assert_eq!(reset["reset"], true);
    assert_eq!(reset["exploration_epoch"], 1);
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
    create_active_lifecycle_profile(&pack, &registry, &token).await;

    // Archive the profile via the lifecycle DAG
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
            "brain.reset",
            json!({"profile_id": LIFECYCLE_PROFILE_ID}),
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
