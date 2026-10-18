// brain.create_profile profile-id grammar enforcement.
#[tokio::test]
async fn r3_create_profile_id_grammar_enforced() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    // Whitespace-only name must be rejected.
    let err = pack
        .dispatch(
            "brain.create_profile",
            json!({"name": "   ", "consumer_kind": "recall"}),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "whitespace-only name must return InvalidInput; got {err:?}"
    );

    // Leading/trailing space: trimmed name "my-profile" must be accepted.
    pack.dispatch(
        "brain.create_profile",
        json!({"name": "  my-profile  ", "consumer_kind": "recall"}),
        &registry,
        &token,
    )
    .await
    .expect("name with leading/trailing spaces should be accepted after trim");

    // Dot in name must be rejected.
    let err = pack
        .dispatch(
            "brain.create_profile",
            json!({"name": "bad.profile", "consumer_kind": "recall"}),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "dot in name must return InvalidInput; got {err:?}"
    );

    // Underscore in name must be rejected.
    let err = pack
        .dispatch(
            "brain.create_profile",
            json!({"name": "bad_profile", "consumer_kind": "recall"}),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "underscore in name must return InvalidInput; got {err:?}"
    );

    // Asterisk in name must be rejected.
    let err = pack
        .dispatch(
            "brain.create_profile",
            json!({"name": "*", "consumer_kind": "recall"}),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "asterisk name must return InvalidInput; got {err:?}"
    );

    // Valid alphanumeric-hyphen name must succeed.
    pack.dispatch(
        "brain.create_profile",
        json!({"name": "valid-profile-123", "consumer_kind": "recall"}),
        &registry,
        &token,
    )
    .await
    .expect("valid alphanumeric-hyphen name must succeed");
}
