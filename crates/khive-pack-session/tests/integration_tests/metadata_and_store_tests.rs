// ── metadata tests ────────────────────────────────────────────────────────────

#[tokio::test]
async fn pack_metadata_matches_trait_consts() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("meta.db"));
    let registry = build_registry(rt);

    let surface: Vec<&str> = registry.all_verbs().iter().map(|v| v.name).collect();
    for verb in [
        "session.store",
        "session.list",
        "session.resume",
        "session.export",
    ] {
        assert!(
            surface.contains(&verb),
            "{verb} must be on the agent-facing verb surface"
        );
        assert!(
            !registry.is_subhandler_verb(verb),
            "{verb} must not be a subhandler"
        );
    }

    assert!(
        registry.all_note_kinds().contains(&"session"),
        "session not in note kinds"
    );
}

// ── session.store tests ───────────────────────────────────────────────────────

#[tokio::test]
async fn store_returns_session_envelope() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("store.db"));
    let registry = build_registry(rt);

    let result = registry
        .dispatch(
            "session.store",
            json!({
                "content": "hello session",
                "title": "My Session",
                "provider": "claude_code",
                "provider_session_id": "abc-123",
                "tags": ["a", "b"]
            }),
        )
        .await
        .expect("store ok");

    assert_eq!(result["ok"], true);
    let session = &result["session"];
    assert_eq!(session["kind"], "session");
    assert!(session["id"].as_str().is_some(), "id must be a string UUID");
    assert_eq!(session["content"], "hello session");
    assert_eq!(session["title"], "My Session");
    assert_eq!(session["provider"], "claude_code");
    assert_eq!(session["provider_session_id"], "abc-123");
    assert_eq!(session["tags"], json!(["a", "b"]));
    assert!(session["created_at"].as_str().is_some());
    assert!(session["updated_at"].as_str().is_some());
}

#[tokio::test]
async fn store_empty_content_returns_error() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("store_empty.db"));
    let registry = build_registry(rt);

    let err = registry
        .dispatch("session.store", json!({"content": ""}))
        .await;
    assert!(err.is_err(), "empty content must return error");
}

#[tokio::test]
async fn store_missing_content_field_returns_error() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("store_missing_content.db"));
    let registry = build_registry(rt);

    let err = registry
        .dispatch("session.store", json!({"title": "no content field"}))
        .await;
    assert!(err.is_err(), "missing required content field must error");
}

#[tokio::test]
async fn store_unknown_field_returns_error() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("store_unknown.db"));
    let registry = build_registry(rt);

    let err = registry
        .dispatch(
            "session.store",
            json!({"content": "x", "unknown_field": true}),
        )
        .await;
    assert!(err.is_err(), "unknown field must be rejected");
}

#[tokio::test]
async fn store_empty_optional_strings_and_tags_rejected() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("store_bad_optionals.db"));
    let registry = build_registry(rt);

    let title_err = registry
        .dispatch("session.store", json!({"content": "x", "title": "  "}))
        .await;
    assert!(title_err.is_err(), "blank title must be rejected");

    let provider_err = registry
        .dispatch("session.store", json!({"content": "x", "provider": ""}))
        .await;
    assert!(provider_err.is_err(), "empty provider must be rejected");

    let tags_err = registry
        .dispatch("session.store", json!({"content": "x", "tags": [""]}))
        .await;
    assert!(tags_err.is_err(), "empty tag entry must be rejected");
}
