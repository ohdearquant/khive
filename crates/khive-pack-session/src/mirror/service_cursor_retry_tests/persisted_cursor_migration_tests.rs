#[tokio::test]
async fn legacy_cursor_schema_adds_identity_without_losing_offset() {
    let (rt, _dir) = runtime_without_schema();
    let mut writer = rt.sql().writer().await.expect("writer");
    writer
            .execute_script(
                "CREATE TABLE session_mirror_cursor (file_path TEXT PRIMARY KEY, session_id TEXT, byte_offset INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL)"
                    .to_string(),
            )
            .await
            .expect("legacy cursor table");
    drop(writer);
    let path = PathBuf::from("/projects/legacy.jsonl");
    insert_cursor_row(&rt, &path, 123).await;

    rt.backend()
        .apply_pack_ddl_statements_with_columns(
            &SESSION_SCHEMA_PLAN_STMTS,
            &SESSION_SCHEMA_COLUMN_ADDITIONS,
        )
        .expect("pack column upgrade");
    let mut cursor = load_cursors(&rt)
        .await
        .expect("load upgraded cursor")
        .remove(&path)
        .expect("legacy row retained");
    assert_eq!(cursor.byte_offset, 123);
    assert_eq!(cursor.file_identity, None);
    assert!(cursor.reset_if_replaced("current-file", 123, true));
    assert_eq!(cursor.byte_offset, 0, "legacy row must replay once");
}

#[tokio::test]
async fn legacy_cursor_without_backfill_keeps_skipped_prefix_and_persists_identity() {
    let (rt, dir) = runtime_without_schema();
    let mut writer = rt.sql().writer().await.expect("writer");
    writer
            .execute_script(
                "CREATE TABLE session_mirror_cursor (file_path TEXT PRIMARY KEY, session_id TEXT, byte_offset INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL)"
                    .to_string(),
            )
            .await
            .expect("legacy cursor table");
    drop(writer);

    let path = dir.path().join("legacy.jsonl");
    let skipped = concat!(
        r#"{"uuid":"event-before-opt-out","sessionId":"sess-legacy","type":"user","timestamp":"2026-08-05T10:00:00Z","message":{"role":"user","content":"skipped"}}"#,
        "\n"
    );
    let appended = concat!(
        r#"{"uuid":"event-after-opt-out","sessionId":"sess-legacy","type":"user","timestamp":"2026-08-05T10:01:00Z","message":{"role":"user","content":"mirrored"}}"#,
        "\n"
    );
    std::fs::write(&path, format!("{skipped}{appended}")).expect("transcript");
    insert_cursor_row(&rt, &path, skipped.len() as i64).await;
    rt.backend()
        .apply_pack_ddl_statements_with_columns(
            &SESSION_SCHEMA_PLAN_STMTS,
            &SESSION_SCHEMA_COLUMN_ADDITIONS,
        )
        .expect("pack column upgrade");

    let mut cursor = load_cursors(&rt)
        .await
        .expect("load upgraded cursor")
        .remove(&path)
        .expect("legacy row retained");
    let file_len = std::fs::metadata(&path).expect("transcript metadata").len();
    let identity = file_identity(&std::fs::File::open(&path).expect("open transcript"))
        .expect("transcript identity");
    assert!(
        !reconcile_cursor_identity(&rt, &path, &mut cursor, &identity, file_len, false)
            .await
            .expect("adopt legacy identity")
    );
    assert_eq!(cursor.byte_offset, skipped.len() as u64);
    assert_eq!(cursor.file_identity.as_deref(), Some(identity.as_str()));

    let saved = load_cursors(&rt)
        .await
        .expect("reload adopted cursor")
        .remove(&path)
        .expect("adopted row");
    assert_eq!(saved, cursor);
    let mirrored = mirror_file(
        &rt,
        &path,
        cursor.byte_offset,
        LineTailSource::ClaudeCode,
        None,
    )
    .await
    .expect("tail after skipped prefix");
    assert_eq!(mirrored.inserted, 1, "only the appended event is imported");

    let mut truncated_legacy = CursorState {
        byte_offset: skipped.len() as u64,
        file_identity: None,
    };
    assert!(truncated_legacy.reset_if_replaced(&identity, 1, false));
    assert_eq!(truncated_legacy.byte_offset, 0);
}
