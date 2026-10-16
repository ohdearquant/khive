fn write_export_file(content: &str) -> (NamedTempFile, std::path::PathBuf) {
    let mut file = NamedTempFile::new().expect("tmpfile");
    write!(file, "{content}").unwrap();
    let path = file.path().to_path_buf();
    (file, path)
}

fn chatgpt_happy_export_json() -> String {
    let conv = json!({
        "id": "conv-happy",
        "title": "Synthetic Happy",
        "create_time": 1_751_462_400.0,
        "current_node": "msg-happy-assistant",
        "mapping": {
            "root-happy": {
                "id": "root-happy",
                "message": null,
                "parent": null,
                "children": ["msg-happy-user"]
            },
            "msg-happy-user": {
                "id": "msg-happy-user",
                "parent": "root-happy",
                "children": ["msg-happy-assistant"],
                "message": {
                    "id": "msg-happy-user",
                    "author": {"role": "user"},
                    "create_time": 1_751_462_400.0,
                    "content": {"content_type": "text", "parts": ["Hello synthetic"]}
                }
            },
            "msg-happy-assistant": {
                "id": "msg-happy-assistant",
                "parent": "msg-happy-user",
                "children": [],
                "message": {
                    "id": "msg-happy-assistant",
                    "author": {"role": "assistant"},
                    "create_time": 1_751_462_401.0,
                    "content": {"content_type": "text", "parts": ["Hi synthetic"]}
                }
            }
        }
    });
    serde_json::to_string(&json!([conv])).unwrap()
}

#[tokio::test]
async fn test_chatgpt_happy_conversations_json() {
    let (rt, _dir) = setup().await;
    let (_file, path) = write_export_file(&chatgpt_happy_export_json());
    let file_len = std::fs::metadata(&path).unwrap().len();

    let stats = mirror_chatgpt_export_file(&rt, &path, 0)
        .await
        .expect("happy path ingest");
    assert_eq!(stats.inserted, 2, "2 message-bearing nodes");
    assert_eq!(stats.scanned, 2, "2 events parsed");
    assert_eq!(stats.new_offset, file_len, "whole-file cursor-at-length");

    let sql = rt.sql();
    let mut r = sql.reader().await.expect("reader");
    let row = r
        .query_row(SqlStatement {
            sql: "SELECT source, slug, cwd, git_branch FROM sessions WHERE id='conv-happy'".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("query ok")
        .expect("session row must exist");
    match row.get("source") {
        Some(SqlValue::Text(s)) => assert_eq!(s, "chatgpt_export"),
        other => panic!("unexpected source: {other:?}"),
    }
    match row.get("slug") {
        Some(SqlValue::Text(s)) => assert_eq!(s, "Synthetic Happy"),
        other => panic!("unexpected slug: {other:?}"),
    }
    assert!(
        matches!(row.get("cwd"), Some(SqlValue::Null) | None),
        "chatgpt export never carries a cwd"
    );
    assert!(
        matches!(row.get("git_branch"), Some(SqlValue::Null) | None),
        "chatgpt export never carries a git branch"
    );

    let mut r2 = sql.reader().await.expect("reader");
    let rows = r2
        .query_all(SqlStatement {
            sql: "SELECT seq, role FROM session_messages \
                      WHERE session_id='conv-happy' ORDER BY seq"
                .into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("query ok");
    let roles: Vec<(i64, String)> = rows
        .iter()
        .map(|row| {
            let seq = match row.get("seq") {
                Some(SqlValue::Integer(n)) => *n,
                other => panic!("unexpected seq: {other:?}"),
            };
            let role = match row.get("role") {
                Some(SqlValue::Text(s)) => s.clone(),
                other => panic!("unexpected role: {other:?}"),
            };
            (seq, role)
        })
        .collect();
    assert_eq!(
        roles,
        vec![(0, "user".to_string()), (1, "assistant".to_string())]
    );
}

fn chatgpt_idempotency_export_json() -> String {
    let conv = json!({
        "id": "conv-idem",
        "title": "Synthetic Idempotency",
        "current_node": "msg-idem-assistant",
        "mapping": {
            "root-idem": {
                "id": "root-idem",
                "message": null,
                "parent": null,
                "children": ["msg-idem-user"]
            },
            "msg-idem-user": {
                "id": "msg-idem-user",
                "parent": "root-idem",
                "children": ["msg-idem-assistant"],
                "message": {
                    "id": "msg-idem-user",
                    "author": {"role": "user"},
                    "content": {"content_type": "text", "parts": ["Same question again"]}
                }
            },
            "msg-idem-assistant": {
                "id": "msg-idem-assistant",
                "parent": "msg-idem-user",
                "children": [],
                "message": {
                    "id": "msg-idem-assistant",
                    "author": {"role": "assistant"},
                    "content": {"content_type": "text", "parts": ["Same answer again"]}
                }
            }
        }
    });
    serde_json::to_string(&json!([conv])).unwrap()
}

#[tokio::test]
async fn test_chatgpt_reingest_idempotency_conversations_json() {
    let (rt, _dir) = setup().await;
    let (_file, path) = write_export_file(&chatgpt_idempotency_export_json());

    let s1 = mirror_chatgpt_export_file(&rt, &path, 0)
        .await
        .expect("first ingest");
    assert_eq!(s1.inserted, 2);

    let seen_after_first = last_seen_at(&rt, "conv-idem")
        .await
        .expect("session row exists");

    // Re-ingest from offset 0 (the service always re-reads the whole file
    // for this source): same event uuids, INSERT OR IGNORE must dedup.
    let s2 = mirror_chatgpt_export_file(&rt, &path, 0)
        .await
        .expect("second ingest");
    assert_eq!(s2.inserted, 0, "re-ingest must insert 0 new rows");

    let sql = rt.sql();
    let mut r = sql.reader().await.expect("reader");
    let count = r
        .query_row(SqlStatement {
            sql: "SELECT COUNT(*) FROM session_messages WHERE session_id='conv-idem'".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("query ok")
        .expect("count row");
    match count.columns.first().map(|c| &c.value) {
        Some(SqlValue::Integer(n)) => assert_eq!(*n, 2, "message count stays at 2"),
        other => panic!("unexpected count: {other:?}"),
    }

    let seen_after_replay = last_seen_at(&rt, "conv-idem")
        .await
        .expect("session row still exists");
    assert_eq!(
        seen_after_first, seen_after_replay,
        "pure replay must not advance last_seen_at"
    );
}
