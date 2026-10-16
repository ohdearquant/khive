/// Build a file-backed runtime (exercises the real `atomic_unit`
/// single-writer path) and apply the session schema. Caller must keep
/// the returned `TempDir` alive.
async fn setup() -> (KhiveRuntime, TempDir) {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("test.db");
    let rt = KhiveRuntime::new(RuntimeConfig {
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: Default::default(),
        wal_ceiling_env_raw: None,
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: khive_runtime::config::resolve_default_display_timezone(),
        events_split: None,
        db_path: Some(db_path),
        blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        exec: Default::default(),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .expect("file-backed runtime");
    apply_session_schema(&rt).await;
    (rt, dir)
}

async fn apply_session_schema(rt: &KhiveRuntime) {
    let sql = rt.sql();
    let mut w = sql.writer().await.expect("writer");
    for stmt in &SESSION_SCHEMA_PLAN_STMTS {
        w.execute_script(stmt.to_string())
            .await
            .expect("schema stmt");
    }
    // w dropped here — releases the writer connection.
}

/// Count rows in a table.
async fn count_rows(rt: &KhiveRuntime, table: &str) -> i64 {
    let sql = rt.sql();
    let mut r = sql.reader().await.expect("reader");
    let row = r
        .query_row(SqlStatement {
            sql: format!("SELECT COUNT(*) FROM {table}"),
            params: vec![],
            label: None,
        })
        .await
        .expect("count query")
        .expect("count row");
    match row.columns.first().map(|c| &c.value) {
        Some(SqlValue::Integer(n)) => *n,
        _ => 0,
    }
}

/// Retrieve the stored byte_offset for a file path.
async fn cursor_offset(rt: &KhiveRuntime, path_str: &str) -> Option<i64> {
    let sql = rt.sql();
    let mut r = sql.reader().await.expect("reader");
    let row = r
        .query_row(SqlStatement {
            sql: "SELECT byte_offset FROM session_mirror_cursor WHERE file_path=?1".into(),
            params: vec![SqlValue::Text(path_str.to_string())],
            label: None,
        })
        .await
        .expect("cursor query")?;
    match row.columns.first().map(|c| &c.value) {
        Some(SqlValue::Integer(n)) => Some(*n),
        _ => None,
    }
}

fn user_line(uuid: &str, session_id: &str, text: &str) -> String {
    format!(
        r#"{{"uuid":"{uuid}","sessionId":"{session_id}","type":"user","timestamp":"2026-06-29T10:00:00Z","message":{{"role":"user","content":"{text}"}}}}"#
    )
}

/// A user line with NO `timestamp` field — `created_at` falls back to `now_us`.
fn user_line_no_ts(uuid: &str, session_id: &str, text: &str) -> String {
    format!(
        r#"{{"uuid":"{uuid}","sessionId":"{session_id}","type":"user","message":{{"role":"user","content":"{text}"}}}}"#
    )
}

/// Retrieve the stored `last_seen_at` for a session id.
async fn last_seen_at(rt: &KhiveRuntime, session_id: &str) -> Option<i64> {
    let sql = rt.sql();
    let mut r = sql.reader().await.expect("reader");
    let row = r
        .query_row(SqlStatement {
            sql: "SELECT last_seen_at FROM sessions WHERE id=?1".into(),
            params: vec![SqlValue::Text(session_id.to_string())],
            label: None,
        })
        .await
        .expect("last_seen query")?;
    match row.columns.first().map(|c| &c.value) {
        Some(SqlValue::Integer(n)) => Some(*n),
        _ => None,
    }
}

#[tokio::test]
async fn test_mirror_three_lines_and_idempotency() {
    let (rt, _dir) = setup().await;

    // Build a fixture JSONL with 3 lines, all ending in '\n'.
    let line1 = user_line("uuid-1", "sess-A", "Hello");
    let line2 = user_line("uuid-2", "sess-A", "World");
    let line3 = user_line("uuid-3", "sess-A", "Done");

    let mut file = NamedTempFile::new().expect("tmpfile");
    writeln!(file, "{line1}").unwrap();
    writeln!(file, "{line2}").unwrap();
    writeln!(file, "{line3}").unwrap();

    let path = file.path().to_path_buf();

    // First call: should insert all 3 rows.
    let stats = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
        .await
        .expect("mirror_file first call");
    assert_eq!(stats.inserted, 3, "should insert 3 new messages");
    assert_eq!(stats.scanned, 3, "should scan 3 lines");
    assert!(stats.new_offset > 0, "offset should advance");

    let msg_count = count_rows(&rt, "session_messages").await;
    assert_eq!(msg_count, 3, "3 messages in DB");

    let session_count = count_rows(&rt, "sessions").await;
    assert_eq!(session_count, 1, "1 session row");

    // Idempotency: second call over the SAME range inserts 0 rows.
    let stats2 = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
        .await
        .expect("mirror_file second call");
    assert_eq!(stats2.inserted, 0, "second pass must insert 0 rows");
    assert_eq!(count_rows(&rt, "session_messages").await, 3);

    // Offset-aware: calling from the advanced offset finds nothing new.
    let stats3 = mirror_file(
        &rt,
        &path,
        stats.new_offset,
        LineTailSource::ClaudeCode,
        None,
    )
    .await
    .expect("mirror_file from new_offset");
    assert_eq!(stats3.inserted, 0, "no new data past advanced offset");
    assert_eq!(stats3.new_offset, stats.new_offset);

    // Cursor was recorded.
    let stored_offset = cursor_offset(&rt, &path.to_string_lossy()).await;
    assert!(stored_offset.is_some(), "cursor should be recorded");
    assert_eq!(stored_offset.unwrap(), stats.new_offset as i64);
}

#[tokio::test]
async fn mirror_file_respects_low_test_cap_and_advances_over_multiple_passes() {
    // PACKSESSION-AUD-003 regression: multi-pass bounded reads (see docs guide).
    let (rt, _dir) = setup().await;

    let lines: Vec<String> = (0..6)
        .map(|i| user_line(&format!("uuid-cap-{i}"), "sess-CAP", &format!("line{i}")))
        .collect();

    let mut file = NamedTempFile::new().expect("tmpfile");
    for line in &lines {
        writeln!(file, "{line}").unwrap();
    }
    let path = file.path().to_path_buf();
    let file_len = std::fs::metadata(&path).unwrap().len();

    // All 6 fixture lines are byte-identical in length, so capping at
    // exactly two lines' worth of bytes forces a 2-line-per-pass split
    // without needing a giant fixture.
    let cap_bytes = (lines[0].len() + 1) + (lines[1].len() + 1);
    let limits = MirrorLimits {
        max_bytes_per_pass: cap_bytes,
        max_events_per_pass: 1024,
        max_line_bytes: MIRROR_MAX_LINE_BYTES,
    };

    let stats1 = mirror_file_with_limits(&rt, &path, 0, LineTailSource::ClaudeCode, None, limits)
        .await
        .expect("first bounded pass");
    assert_eq!(
        stats1.inserted, 2,
        "first pass must stop at the byte cap, not read the whole file"
    );
    assert_eq!(stats1.scanned, 2);
    assert!(
        stats1.new_offset < file_len,
        "new_offset {new} must be less than file_len {file_len} for a bounded pass",
        new = stats1.new_offset
    );
    assert_eq!(
        cursor_offset(&rt, &path.to_string_lossy()).await,
        Some(stats1.new_offset as i64),
        "cursor must be committed after the first bounded pass"
    );

    let stats2 = mirror_file_with_limits(
        &rt,
        &path,
        stats1.new_offset,
        LineTailSource::ClaudeCode,
        None,
        limits,
    )
    .await
    .expect("second bounded pass");
    assert_eq!(stats2.inserted, 2);
    assert!(stats2.new_offset > stats1.new_offset);
    assert!(stats2.new_offset < file_len);

    let stats3 = mirror_file_with_limits(
        &rt,
        &path,
        stats2.new_offset,
        LineTailSource::ClaudeCode,
        None,
        limits,
    )
    .await
    .expect("third bounded pass");
    assert_eq!(stats3.inserted, 2);
    assert_eq!(stats3.new_offset, file_len, "final pass must reach EOF");

    // All 6 rows landed across 3 bounded passes, and the cursor reflects
    // the full file — no pass allocated or inserted the entire file at
    // once.
    assert_eq!(count_rows(&rt, "session_messages").await, 6);
    assert_eq!(
        cursor_offset(&rt, &path.to_string_lossy()).await,
        Some(file_len as i64)
    );

    // A pass with no remaining bytes is a clean no-op.
    let stats4 = mirror_file_with_limits(
        &rt,
        &path,
        stats3.new_offset,
        LineTailSource::ClaudeCode,
        None,
        limits,
    )
    .await
    .expect("fourth pass at EOF");
    assert_eq!(stats4.inserted, 0);
    assert_eq!(stats4.scanned, 0);
}
