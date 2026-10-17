#[test]
fn cursor_preserves_unchanged_and_append_offsets() {
    let mut cursor = CursorState {
        byte_offset: 100,
        file_identity: Some("old-file".into()),
    };
    assert!(!cursor.reset_if_replaced("old-file", 100, true));
    assert!(!cursor.reset_if_replaced("old-file", 150, true));
    assert_eq!(cursor.byte_offset, 100, "an append keeps its offset");
}

#[test]
fn cursor_truncation_rewinds_from_previous_offset() {
    let mut cursor = CursorState {
        byte_offset: 100,
        file_identity: Some("old-file".into()),
    };
    assert!(cursor.reset_if_replaced("old-file", 50, true));
    assert_eq!(cursor.byte_offset, 0, "truncation restarts from zero");
}

#[test]
fn cursor_identity_change_rewinds_without_truncation() {
    for backfill in [true, false] {
        let mut cursor = CursorState {
            byte_offset: 100,
            file_identity: Some("old-file".into()),
        };
        assert!(cursor.reset_if_replaced("new-file", 150, backfill));
        assert_eq!(
            cursor.byte_offset, 0,
            "a replacement at least as long as the offset restarts from zero"
        );
        assert_eq!(cursor.file_identity.as_deref(), Some("new-file"));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn same_size_atomic_replacement_is_reingested_after_cursor_reload() {
    let (rt, dir) = runtime_without_schema();
    apply_session_schema(&rt).await;
    let path = dir.path().join("transcript.jsonl");
    let original = concat!(
        r#"{"uuid":"event-a","sessionId":"sess-replace","type":"user","timestamp":"2026-08-05T10:00:00Z","message":{"role":"user","content":"first"}}"#,
        "\n"
    );
    let replacement = original.replace("event-a", "event-b");
    assert_eq!(original.len(), replacement.len());
    std::fs::write(&path, original).expect("original transcript");
    let first = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
        .await
        .expect("first ingest");
    assert_eq!(first.inserted, 1);

    let mut cursor = load_cursors(&rt)
        .await
        .expect("reload cursor")
        .remove(&path)
        .expect("stored cursor");
    assert_eq!(cursor.byte_offset, original.len() as u64);
    assert_eq!(
        cursor.file_identity.as_deref(),
        Some(
            file_identity(&std::fs::File::open(&path).expect("open original"))
                .expect("original identity")
                .as_str()
        )
    );
    assert!(!cursor.reset_if_replaced(
        &file_identity(&std::fs::File::open(&path).expect("open unchanged"))
            .expect("unchanged identity"),
        original.len() as u64,
        true,
    ));

    let staged = dir.path().join("replacement.jsonl");
    std::fs::write(&staged, replacement).expect("replacement transcript");
    std::fs::rename(&staged, &path).expect("atomic replacement");
    let new_identity = file_identity(&std::fs::File::open(&path).expect("open replacement"))
        .expect("replacement identity");
    assert!(cursor.reset_if_replaced(&new_identity, original.len() as u64, true));
    assert_eq!(cursor.byte_offset, 0);
    let second = mirror_file(
        &rt,
        &path,
        cursor.byte_offset,
        LineTailSource::ClaudeCode,
        None,
    )
    .await
    .expect("replacement ingest");
    assert_eq!(second.inserted, 1);
    let saved = load_cursors(&rt)
        .await
        .expect("reload replacement cursor")
        .remove(&path)
        .expect("replacement cursor");
    assert_eq!(saved.byte_offset, original.len() as u64);
    assert_eq!(saved.file_identity.as_deref(), Some(new_identity.as_str()));
}
