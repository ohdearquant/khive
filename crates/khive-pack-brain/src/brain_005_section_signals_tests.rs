use super::*;

#[tokio::test]
async fn section_signals_not_an_object_is_rejected() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    let err = pack
        .dispatch(
            "brain.feedback",
            json!({
                "target_id": target,
                "signal": "useful",
                "section_signals": ["overview", "useful"]
            }),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    if let RuntimeError::InvalidInput(msg) = &err {
        assert!(
            msg.contains("section_signals"),
            "error must mention section_signals; got: {msg}"
        );
    } else {
        panic!("expected InvalidInput, got {err:?}");
    }
}

#[tokio::test]
async fn section_signals_unknown_section_is_rejected() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    let err = pack
        .dispatch(
            "brain.feedback",
            json!({
                "target_id": target,
                "signal": "useful",
                "section_signals": {"not_a_real_section": "useful"}
            }),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    if let RuntimeError::InvalidInput(msg) = &err {
        assert!(
            msg.contains("not_a_real_section"),
            "error must name the bad key; got: {msg}"
        );
        assert!(
            msg.contains("valid") || msg.contains("overview"),
            "error must list valid sections; got: {msg}"
        );
    } else {
        panic!("expected InvalidInput, got {err:?}");
    }
}

#[tokio::test]
async fn section_signals_unknown_signal_value_is_rejected() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    let err = pack
        .dispatch(
            "brain.feedback",
            json!({
                "target_id": target,
                "signal": "useful",
                "section_signals": {"overview": "garbage_signal"}
            }),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    if let RuntimeError::InvalidInput(msg) = &err {
        assert!(
            msg.contains("garbage_signal"),
            "error must name the bad signal; got: {msg}"
        );
    } else {
        panic!("expected InvalidInput, got {err:?}");
    }
}

#[tokio::test]
async fn section_signals_non_string_value_is_rejected() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    let err = pack
        .dispatch(
            "brain.feedback",
            json!({
                "target_id": target,
                "signal": "useful",
                "section_signals": {"overview": 42}
            }),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    if let RuntimeError::InvalidInput(msg) = &err {
        assert!(
            msg.contains("section_signals"),
            "error must mention section_signals; got: {msg}"
        );
        assert!(
            msg.contains("useful") || msg.contains("not_useful"),
            "error for non-string signal must name valid values; got: {msg}"
        );
    } else {
        panic!("expected InvalidInput, got {err:?}");
    }
}

#[tokio::test]
async fn section_signals_valid_object_is_accepted() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    let result = pack
        .dispatch(
            "brain.feedback",
            json!({
                "target_id": target,
                "signal": "useful",
                "section_signals": {
                    "overview": "useful",
                    "formalism": "not_useful"
                }
            }),
            &registry,
            &token,
        )
        .await
        .expect("valid section_signals must be accepted");
    assert_eq!(result["emitted"], json!(true));
}

// ── PR #46 regression tests ──────────────────────────────────────────────

#[tokio::test]
async fn section_signals_empty_map_is_rejected() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    // An empty {} carries no evidence and must not advance posterior state.
    let err = pack
        .dispatch(
            "brain.feedback",
            json!({
                "target_id": target,
                "signal": "useful",
                "section_signals": {}
            }),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    if let RuntimeError::InvalidInput(msg) = &err {
        assert!(
            msg.contains("section_signals"),
            "error must mention section_signals; got: {msg}"
        );
    } else {
        panic!("expected InvalidInput for empty section_signals, got {err:?}");
    }
}

#[tokio::test]
async fn section_signals_semantic_signal_value_is_rejected() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    // Section fold (ADR-048) only handles useful|not_useful|wrong.
    // Semantic event kinds belong to the profile-level signal, not section values.
    for semantic_value in [
        "explicit_positive",
        "explicit_negative",
        "implicit_positive",
        "implicit_negative",
        "correction",
    ] {
        let err = pack
            .dispatch(
                "brain.feedback",
                json!({
                    "target_id": target,
                    "signal": "useful",
                    "section_signals": {"overview": semantic_value}
                }),
                &registry,
                &token,
            )
            .await
            .unwrap_err();
        if let RuntimeError::InvalidInput(msg) = &err {
            assert!(
                msg.contains(semantic_value),
                "error must name the invalid signal {semantic_value:?}; got: {msg}"
            );
            assert!(
                msg.contains("useful") || msg.contains("not_useful"),
                "error must list valid section signals; got: {msg}"
            );
        } else {
            panic!(
                "expected InvalidInput for semantic section signal {semantic_value:?}, got {err:?}"
            );
        }
    }
}
