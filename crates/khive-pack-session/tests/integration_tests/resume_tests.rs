// ── session.resume tests ───────────────────────────────────────────────────────

#[tokio::test]
async fn resume_by_full_uuid_returns_exact_content() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("resume.db"));
    let registry = build_registry(rt);

    let stored = store_session(&registry, "resumeable content").await;
    let id = stored["session"]["id"]
        .as_str()
        .expect("id present")
        .to_string();

    let resumed = registry
        .dispatch("session.resume", json!({"id": id}))
        .await
        .expect("resume ok");

    assert_eq!(resumed["ok"], true);
    assert_eq!(resumed["session"]["id"], id);
    assert_eq!(resumed["session"]["kind"], "session");
    assert_eq!(resumed["session"]["content"], "resumeable content");
}

#[tokio::test]
async fn resume_by_short_prefix_returns_same_note() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("resume_prefix.db"));
    let registry = build_registry(rt);

    let stored = store_session(&registry, "prefix content").await;
    let id = stored["session"]["id"]
        .as_str()
        .expect("id present")
        .to_string();
    let prefix = &id.replace('-', "")[..8];

    let resumed = registry
        .dispatch("session.resume", json!({"id": prefix}))
        .await
        .expect("resume by prefix ok");

    assert_eq!(resumed["session"]["id"], id);
    assert_eq!(resumed["session"]["content"], "prefix content");
}

#[tokio::test]
async fn resume_rejects_non_uuid_non_prefix() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("resume_bad_id.db"));
    let registry = build_registry(rt);

    let err = registry
        .dispatch("session.resume", json!({"id": "abc"}))
        .await;
    assert!(err.is_err(), "short non-hex id must be rejected");
}

#[tokio::test]
async fn resume_rejects_well_formed_but_nonexistent_uuid() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("resume_missing_uuid.db"));
    let registry = build_registry(rt);

    let err = registry
        .dispatch(
            "session.resume",
            json!({"id": "00000000-0000-0000-0000-000000000000"}),
        )
        .await;
    assert!(
        err.is_err(),
        "well-formed UUID with no matching record must error"
    );
}

#[tokio::test]
async fn resume_rejects_hex_prefix_matching_nothing() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("resume_bad_prefix.db"));
    let registry = build_registry(rt);

    let err = registry
        .dispatch("session.resume", json!({"id": "deadbeef"}))
        .await;
    assert!(err.is_err(), "8+ hex prefix matching no records must error");
}

#[tokio::test]
async fn resume_rejects_non_session_note() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("resume_wrong_kind.db"));
    let registry = build_registry(rt);

    let observation = registry
        .dispatch(
            "create",
            json!({"kind": "observation", "content": "not a session", "name": "obs"}),
        )
        .await
        .expect("create observation ok");
    let observation_id = observation["id"]
        .as_str()
        .expect("observation id")
        .to_string();

    let err = registry
        .dispatch("session.resume", json!({"id": observation_id}))
        .await;
    assert!(err.is_err(), "non-session note must be rejected");
}

#[tokio::test]
async fn resume_excludes_soft_deleted_session() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("resume_deleted.db"));
    let registry = build_registry(rt);

    let stored = store_session(&registry, "deleted resume session").await;
    let id = stored["session"]["id"].as_str().expect("id").to_string();
    registry
        .dispatch("delete", json!({"id": id}))
        .await
        .expect("soft delete session");

    let result = registry.dispatch("session.resume", json!({"id": id})).await;
    assert!(
        matches!(&result, Err(RuntimeError::NotFound(_))),
        "resume must report a soft-deleted session as not found: {result:?}"
    );
}
