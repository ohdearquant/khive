// ── session.export tests ──────────────────────────────────────────────────────

#[tokio::test]
async fn export_default_json() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("export_json.db"));
    let registry = build_registry(rt);

    let stored = store_session(&registry, "exportable content").await;
    let id = stored["session"]["id"]
        .as_str()
        .expect("id present")
        .to_string();

    let exported = registry
        .dispatch("session.export", json!({"id": id}))
        .await
        .expect("export ok");

    assert_eq!(exported["ok"], true);
    assert_eq!(exported["format"], "json");
    assert_eq!(exported["session"]["content"], "exportable content");
}

#[tokio::test]
async fn export_markdown_contains_metadata_and_content() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("export_md.db"));
    let registry = build_registry(rt);

    let stored = registry
        .dispatch(
            "session.store",
            json!({"content": "md body", "title": "MD Title", "provider": "codex"}),
        )
        .await
        .expect("store ok");
    let id = stored["session"]["id"]
        .as_str()
        .expect("id present")
        .to_string();

    let exported = registry
        .dispatch("session.export", json!({"id": id, "format": "markdown"}))
        .await
        .expect("export markdown ok");

    assert_eq!(exported["ok"], true);
    assert_eq!(exported["format"], "markdown");
    let content = exported["content"].as_str().expect("content string");
    assert!(content.starts_with("# MD Title"));
    assert!(content.contains("- provider: codex"));
    assert!(content.contains("## Content"));
    assert!(content.contains("md body"));
}

#[tokio::test]
async fn export_rejects_invalid_format() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("export_bad_format.db"));
    let registry = build_registry(rt);

    let stored = store_session(&registry, "content").await;
    let id = stored["session"]["id"]
        .as_str()
        .expect("id present")
        .to_string();

    let err = registry
        .dispatch("session.export", json!({"id": id, "format": "text"}))
        .await;
    assert!(err.is_err(), "format=text must be rejected");
}

#[tokio::test]
async fn export_rejects_missing_id() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("export_missing_id.db"));
    let registry = build_registry(rt);

    let err = registry
        .dispatch("session.export", json!({"format": "json"}))
        .await;
    assert!(err.is_err(), "missing required id field must error");
}

#[tokio::test]
async fn export_excludes_soft_deleted_session() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("export_deleted.db"));
    let registry = build_registry(rt);

    let stored = store_session(&registry, "deleted export session").await;
    let id = stored["session"]["id"].as_str().expect("id").to_string();
    registry
        .dispatch("delete", json!({"id": id}))
        .await
        .expect("soft delete session");

    let result = registry.dispatch("session.export", json!({"id": id})).await;
    assert!(
        matches!(&result, Err(RuntimeError::NotFound(_))),
        "export must report a soft-deleted session as not found: {result:?}"
    );
}

// ── round-trip regression ────────────────────────────────────────────────────

#[tokio::test]
async fn round_trip_store_resume_list_export() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt(dir.path().join("round_trip.db"));
    let registry = build_registry(rt);

    let stored = registry
        .dispatch(
            "session.store",
            json!({
                "content": "round trip content",
                "provider": "codex",
                "provider_session_id": "codex-session-1"
            }),
        )
        .await
        .expect("store ok");
    let id = stored["session"]["id"]
        .as_str()
        .expect("id present")
        .to_string();

    let resumed = registry
        .dispatch("session.resume", json!({"id": id}))
        .await
        .expect("resume ok");
    assert_eq!(resumed["session"]["content"], "round trip content");

    let listed = registry
        .dispatch("session.list", json!({}))
        .await
        .expect("list ok");
    let ids: Vec<&str> = listed["sessions"]
        .as_array()
        .expect("sessions array")
        .iter()
        .map(|s| s["id"].as_str().expect("id"))
        .collect();
    assert!(ids.contains(&id.as_str()));

    let exported = registry
        .dispatch("session.export", json!({"id": id}))
        .await
        .expect("export ok");
    assert!(
        exported["session"].is_object(),
        "export json must parse as an object"
    );
}
