fn chatgpt_branch_sidechain_export_json() -> String {
    let conv = json!({
        "id": "conv-branch",
        "title": "Synthetic Branch",
        "current_node": "msg-branch-main",
        "mapping": {
            "root-branch": {
                "id": "root-branch",
                "message": null,
                "parent": null,
                "children": ["msg-branch-user"]
            },
            "msg-branch-user": {
                "id": "msg-branch-user",
                "parent": "root-branch",
                "children": ["msg-branch-main", "msg-branch-alt"],
                "message": {
                    "id": "msg-branch-user",
                    "author": {"role": "user"},
                    "content": {"content_type": "text", "parts": ["Branch question"]}
                }
            },
            "msg-branch-main": {
                "id": "msg-branch-main",
                "parent": "msg-branch-user",
                "children": [],
                "message": {
                    "id": "msg-branch-main",
                    "author": {"role": "assistant"},
                    "content": {"content_type": "text", "parts": ["Main answer"]}
                }
            },
            "msg-branch-alt": {
                "id": "msg-branch-alt",
                "parent": "msg-branch-user",
                "children": [],
                "message": {
                    "id": "msg-branch-alt",
                    "author": {"role": "assistant"},
                    "content": {"content_type": "text", "parts": ["Alternate answer"]}
                }
            }
        }
    });
    serde_json::to_string(&json!([conv])).unwrap()
}

#[tokio::test]
async fn test_chatgpt_branch_sidechain_conversations_json() {
    let (rt, _dir) = setup().await;
    let (_file, path) = write_export_file(&chatgpt_branch_sidechain_export_json());

    let stats = mirror_chatgpt_export_file(&rt, &path, 0)
        .await
        .expect("branch ingest");
    assert_eq!(stats.inserted, 3, "user + main + alt all stored");

    let sql = rt.sql();
    let mut r = sql.reader().await.expect("reader");
    let rows = r
        .query_all(SqlStatement {
            sql: "SELECT id, is_sidechain, text FROM session_messages \
                      WHERE session_id='conv-branch' ORDER BY id"
                .into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("query ok");
    assert_eq!(rows.len(), 3);

    for row in &rows {
        let id = match row.get("id") {
            Some(SqlValue::Text(s)) => s.clone(),
            other => panic!("unexpected id: {other:?}"),
        };
        let is_sidechain = match row.get("is_sidechain") {
            Some(SqlValue::Integer(n)) => *n,
            other => panic!("unexpected is_sidechain: {other:?}"),
        };
        let text = match row.get("text") {
            Some(SqlValue::Text(s)) => s.clone(),
            other => panic!("unexpected text: {other:?}"),
        };
        match id.as_str() {
            "msg-branch-user" | "msg-branch-main" => {
                assert_eq!(is_sidechain, 0, "{id} is on the current-node path")
            }
            "msg-branch-alt" => {
                assert_eq!(is_sidechain, 1, "alt branch is off the current-node path");
                assert_eq!(
                    text, "Alternate answer",
                    "sidechain content must be preserved, not dropped"
                );
            }
            other => panic!("unexpected message id: {other}"),
        }
    }
}

#[tokio::test]
async fn test_chatgpt_malformed_conversations_json_cursor_does_not_advance() {
    let (rt, _dir) = setup().await;

    // Seed the path with a valid (if empty) export and record its cursor.
    let (mut file, path) = write_export_file("[]");
    let seeded_stats = mirror_chatgpt_export_file(&rt, &path, 0)
        .await
        .expect("seeding with an empty array is a valid parse");
    assert_eq!(seeded_stats.inserted, 0);
    let seeded_offset = seeded_stats.new_offset;

    let seeded_sessions = count_rows(&rt, "sessions").await;
    let seeded_messages = count_rows(&rt, "session_messages").await;

    // Overwrite with a longer, malformed (valid-JSON-but-not-an-array) body.
    let malformed = r#"{"oops": "not a chatgpt export array"}"#;
    file.as_file_mut().set_len(0).expect("truncate");
    std::io::Seek::seek(file.as_file_mut(), std::io::SeekFrom::Start(0)).unwrap();
    write!(file, "{malformed}").unwrap();

    let result = mirror_chatgpt_export_file(&rt, &path, seeded_offset).await;
    assert!(
        matches!(result, Err(RuntimeError::Internal(_))),
        "malformed export must return Internal error, got {result:?}"
    );

    let stored_offset = cursor_offset(&rt, &path.to_string_lossy()).await;
    assert_eq!(
        stored_offset,
        Some(seeded_offset as i64),
        "cursor must remain at the pre-error value"
    );
    assert_eq!(
        count_rows(&rt, "sessions").await,
        seeded_sessions,
        "no new session rows on parse failure"
    );
    assert_eq!(
        count_rows(&rt, "session_messages").await,
        seeded_messages,
        "no new message rows on parse failure"
    );
}

#[tokio::test]
async fn test_chatgpt_export_over_max_bytes_is_skipped_without_reading() {
    // PACKSESSION-AUD-003 regression: oversized ChatGPT exports are
    // skipped without reading (see docs guide).
    let (rt, _dir) = setup().await;
    let (_file, path) = write_export_file("[]");

    let file_len = std::fs::metadata(&path).unwrap().len();
    let max_bytes = 1u64; // smaller than even an empty-array export
    assert!(
        file_len > max_bytes,
        "fixture export must exceed the tiny ceiling"
    );

    let stats = mirror_chatgpt_export_file_with_max_bytes(&rt, &path, 0, max_bytes)
        .await
        .expect("an oversized export must be skipped, not error");

    assert_eq!(stats.inserted, 0);
    assert_eq!(stats.scanned, 0);
    assert_eq!(
        stats.new_offset, 0,
        "cursor must not advance past a skipped oversized export"
    );
    assert_eq!(
        cursor_offset(&rt, &path.to_string_lossy()).await,
        None,
        "no cursor row should be written for a skipped pass"
    );
    assert_eq!(count_rows(&rt, "sessions").await, 0);
    assert_eq!(count_rows(&rt, "session_messages").await, 0);
}
