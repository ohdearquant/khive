// H4: brain.profile accepts profile_id (canonical) and id (alias).
#[tokio::test]
async fn w4_h4_profile_accepts_profile_id_and_id_alias() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    // Canonical arg.
    let r1 = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(r1["id"], json!("balanced-recall-v1"));

    // Legacy alias.
    let r2 = pack
        .dispatch(
            "brain.profile",
            json!({"id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(r2["id"], json!("balanced-recall-v1"));
}

// ── Regression tests ──────────────────────────────────────────────

#[tokio::test]
async fn default_profile_cannot_be_deactivated_or_archived() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    for verb in ["brain.deactivate", "brain.archive"] {
        let error = pack
            .dispatch(
                verb,
                json!({"profile_id": "balanced-recall-v1"}),
                &registry,
                &token,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, RuntimeError::InvalidInput(ref message) if message.contains("required default profile")),
            "{verb} must refuse to disable the sole default: {error:?}"
        );
    }
    let profile = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(profile["lifecycle"], json!("active"));
}
