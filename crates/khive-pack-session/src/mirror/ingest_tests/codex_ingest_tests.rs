// ── Codex source integration tests ────────────────────────────────────────

/// Build a minimal Codex response_item/message line (`input_text` for
/// user, `output_text` for assistant — the generic `text` type does not
/// occur in real Codex transcripts).
fn codex_message_line(role: &str, text: &str) -> String {
    let block_type = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    format!(
        r#"{{"type":"response_item","timestamp":"2026-06-30T08:00:00Z","payload":{{"type":"message","role":"{role}","content":[{{"type":"{block_type}","text":"{text}"}}]}}}}"#
    )
}

/// Build a minimal Codex session_meta line.
fn codex_meta_line(session_id: &str, cwd: &str, branch: &str) -> String {
    format!(
        r#"{{"type":"session_meta","timestamp":"2026-06-30T08:00:00Z","payload":{{"id":"{session_id}","cwd":"{cwd}","git":{{"branch":"{branch}","commit_hash":"abc","repository_url":"https://github.com/example/repo"}}}}}}"#
    )
}

/// Build a Codex event_msg line (should be skipped).
fn codex_event_msg_line() -> String {
    r#"{"type":"event_msg","timestamp":"2026-06-30T08:00:00Z","payload":{"type":"user_message","content":"should be skipped"}}"#.to_string()
}

#[tokio::test]
async fn test_codex_mirror_inserts_with_source_codex() {
    let (rt, _dir) = setup().await;

    let session_id = "cdx-sess-0001-0001-0001-000000000001";
    let meta = codex_meta_line(session_id, "/home/lion/proj", "feat-x");
    let user_msg = codex_message_line("user", "Hello from Codex");
    let asst_msg = codex_message_line("assistant", "Hello back from Codex");
    let skip_msg = codex_event_msg_line();

    let mut file = NamedTempFile::new().expect("tmpfile");
    writeln!(file, "{meta}").unwrap();
    writeln!(file, "{user_msg}").unwrap();
    writeln!(file, "{asst_msg}").unwrap();
    writeln!(file, "{skip_msg}").unwrap();

    let path = file.path().to_path_buf();

    // Mirror the file as Codex source.
    let stats = mirror_file(&rt, &path, 0, LineTailSource::Codex, Some(session_id))
        .await
        .expect("codex mirror_file");

    // session_meta + 2 response_item/message rows = 3 parseable, event_msg skipped.
    assert_eq!(stats.inserted, 3, "meta + 2 messages inserted");
    assert_eq!(
        stats.scanned, 4,
        "4 lines total (including skipped event_msg)"
    );
    assert!(stats.new_offset > 0);

    // Session row exists with source='codex'.
    let sql = rt.sql();
    let mut r = sql.reader().await.expect("reader");
    let session_row = r
        .query_row(SqlStatement {
            sql: "SELECT source FROM sessions WHERE id=?1".into(),
            params: vec![SqlValue::Text(session_id.to_string())],
            label: None,
        })
        .await
        .expect("query ok")
        .expect("session row must exist");
    match session_row.columns.first().map(|c| &c.value) {
        Some(SqlValue::Text(s)) => assert_eq!(s, "codex", "source must be 'codex'"),
        other => panic!("unexpected source value: {other:?}"),
    }

    // All 3 message rows are stored.
    assert_eq!(count_rows(&rt, "session_messages").await, 3);

    // The two response_item/message rows carry their real input_text/
    // output_text content through to session_messages.text — not just a
    // row count, but the actual extracted string for each role.
    let mut r2 = sql.reader().await.expect("reader");
    let rows = r2
        .query_all(SqlStatement {
            sql: "SELECT role, text FROM session_messages \
                      WHERE session_id=?1 AND role IS NOT NULL ORDER BY seq"
                .into(),
            params: vec![SqlValue::Text(session_id.to_string())],
            label: None,
        })
        .await
        .expect("query ok");
    let texts: Vec<(String, String)> = rows
        .iter()
        .map(|row| {
            let role = match row.get("role") {
                Some(SqlValue::Text(s)) => s.clone(),
                other => panic!("unexpected role value: {other:?}"),
            };
            let text = match row.get("text") {
                Some(SqlValue::Text(s)) => s.clone(),
                other => panic!("unexpected text value: {other:?}"),
            };
            (role, text)
        })
        .collect();
    assert_eq!(
        texts,
        vec![
            ("user".to_string(), "Hello from Codex".to_string()),
            ("assistant".to_string(), "Hello back from Codex".to_string()),
        ],
        "input_text/output_text blocks must round-trip to session_messages.text by role"
    );
}

#[tokio::test]
async fn test_codex_event_id_is_stable_and_idempotent() {
    // Verifies that: (a) synthetic uuid format is "{session_id}:{offset}",
    // and (b) a second mirror_file pass over the same bytes inserts 0 rows.
    let (rt, _dir) = setup().await;

    let session_id = "cdx-sess-idem-0001-0001-000000000002";
    let user_msg = codex_message_line("user", "Idempotency test");

    let mut file = NamedTempFile::new().expect("tmpfile");
    writeln!(file, "{user_msg}").unwrap();

    let path = file.path().to_path_buf();

    // First pass.
    let s1 = mirror_file(&rt, &path, 0, LineTailSource::Codex, Some(session_id))
        .await
        .unwrap();
    assert_eq!(s1.inserted, 1);

    // Verify the stored id matches the expected synthetic format.
    let sql = rt.sql();
    let mut r = sql.reader().await.expect("reader");
    let msg_row = r
        .query_row(SqlStatement {
            sql: "SELECT id FROM session_messages WHERE session_id=?1".into(),
            params: vec![SqlValue::Text(session_id.to_string())],
            label: None,
        })
        .await
        .expect("query ok")
        .expect("message row must exist");
    let stored_id = match msg_row.columns.first().map(|c| &c.value) {
        Some(SqlValue::Text(s)) => s.clone(),
        other => panic!("unexpected id type: {other:?}"),
    };
    // The line starts at byte offset 0.
    let expected_id = format!("{session_id}:0");
    assert_eq!(
        stored_id, expected_id,
        "synthetic uuid must be {{session_id}}:{{offset}}"
    );

    // Second pass from offset 0: same lines, 0 new rows (idempotent).
    let s2 = mirror_file(&rt, &path, 0, LineTailSource::Codex, Some(session_id))
        .await
        .unwrap();
    assert_eq!(s2.inserted, 0, "second pass must be idempotent");
    assert_eq!(count_rows(&rt, "session_messages").await, 1);

    // Incremental pass from advanced offset: no new data.
    let s3 = mirror_file(
        &rt,
        &path,
        s1.new_offset,
        LineTailSource::Codex,
        Some(session_id),
    )
    .await
    .unwrap();
    assert_eq!(s3.inserted, 0, "incremental pass finds nothing new");
}

#[tokio::test]
async fn test_codex_and_cc_are_independent_sessions() {
    // Both sources can coexist in the same DB; source column distinguishes them.
    let (rt, _dir) = setup().await;

    let cc_line = user_line("cc-uuid-1", "cc-sess-1", "from claude code");
    let mut cc_file = NamedTempFile::new().expect("cc tmpfile");
    writeln!(cc_file, "{cc_line}").unwrap();

    let cdx_session_id = "cdx-sess-coex-0001-0001-000000000003";
    let cdx_msg = codex_message_line("user", "from codex");
    let mut cdx_file = NamedTempFile::new().expect("cdx tmpfile");
    writeln!(cdx_file, "{cdx_msg}").unwrap();

    mirror_file(&rt, cc_file.path(), 0, LineTailSource::ClaudeCode, None)
        .await
        .unwrap();

    mirror_file(
        &rt,
        cdx_file.path(),
        0,
        LineTailSource::Codex,
        Some(cdx_session_id),
    )
    .await
    .unwrap();

    assert_eq!(count_rows(&rt, "sessions").await, 2);
    assert_eq!(count_rows(&rt, "session_messages").await, 2);

    // Verify sources are distinct.
    let sql = rt.sql();
    let mut r = sql.reader().await.expect("reader");
    let rows = r
        .query_all(SqlStatement {
            sql: "SELECT source FROM sessions ORDER BY source".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("query ok");
    let sources: Vec<String> = rows
        .iter()
        .filter_map(|row| match row.get("source") {
            Some(SqlValue::Text(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(sources, vec!["claude_code", "codex"]);
}

// ── ChatGPT export whole-file ingest tests ────────────────────────────────
//
// All fixtures below are hand-authored synthetic JSON, not real export
// content. Node ids are set equal to their own `message.id` so that
// `parent_uuid` (which threads through the mapping node id, per
// `parse::build_chatgpt_event`) resolves to the expected message uuid.

use serde_json::json;
