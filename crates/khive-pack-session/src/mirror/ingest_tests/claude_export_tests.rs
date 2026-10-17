fn claude_ai_happy_export_json() -> String {
    serde_json::to_string(&json!([{
        "uuid": "claude-conv-happy",
        "name": "Synthetic Claude.ai Export",
        "created_at": "2026-07-31T10:00:00Z",
        "current_leaf_message_uuid": "claude-msg-main",
        "chat_messages": [
            {
                "uuid": "claude-msg-user",
                "sender": "human",
                "index": 0,
                "parent_message_uuid": "00000000-0000-4000-8000-000000000000",
                "created_at": "2026-07-31T10:00:01Z",
                "content": [{"type": "text", "text": "Question"}]
            },
            {
                "uuid": "claude-msg-main",
                "sender": "assistant",
                "index": 1,
                "parent_message_uuid": "claude-msg-user",
                "created_at": "2026-07-31T10:00:02Z",
                "content": [{"type": "text", "text": "Current answer"}]
            },
            {
                "uuid": "claude-msg-alt",
                "sender": "assistant",
                "index": 2,
                "parent_message_uuid": "claude-msg-user",
                "created_at": "2026-07-31T10:00:03Z",
                "content": [{"type": "text", "text": "Alternate answer"}]
            }
        ]
    }]))
    .unwrap()
}

#[tokio::test]
async fn test_claude_ai_export_ingest_is_idempotent_and_preserves_branches() {
    let (rt, _dir) = setup().await;
    let (_file, path) = write_export_file(&claude_ai_happy_export_json());
    let file_len = std::fs::metadata(&path).unwrap().len();

    let first = mirror_claude_ai_export_file(&rt, &path, 0)
        .await
        .expect("claude.ai export ingest");
    assert_eq!(first.inserted, 3);
    assert_eq!(first.scanned, 3);
    assert_eq!(first.new_offset, file_len);

    let replay = mirror_claude_ai_export_file(&rt, &path, 0)
        .await
        .expect("idempotent replay");
    assert_eq!(replay.inserted, 0);
    assert_eq!(count_rows(&rt, "sessions").await, 1);
    assert_eq!(count_rows(&rt, "session_messages").await, 3);
    assert_eq!(
        cursor_offset(&rt, &path.to_string_lossy()).await,
        Some(file_len as i64)
    );

    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let session = reader
        .query_row(SqlStatement {
            sql: "SELECT provider_session_id, source, slug, message_count \
                      FROM sessions WHERE id='claude-conv-happy'"
                .into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("query ok")
        .expect("session row");
    assert!(matches!(
        session.get("provider_session_id"),
        Some(SqlValue::Text(value)) if value == "claude-conv-happy"
    ));
    assert!(matches!(
        session.get("source"),
        Some(SqlValue::Text(value)) if value == "claude_ai_export"
    ));
    assert!(matches!(
        session.get("slug"),
        Some(SqlValue::Text(value)) if value == "Synthetic Claude.ai Export"
    ));
    assert!(matches!(
        session.get("message_count"),
        Some(SqlValue::Integer(3))
    ));

    let messages = reader
        .query_all(SqlStatement {
            sql: "SELECT id, parent_uuid, is_sidechain FROM session_messages \
                      WHERE session_id='claude-conv-happy' ORDER BY seq"
                .into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("message query");
    assert_eq!(messages.len(), 3);
    assert!(matches!(
        messages[0].get("parent_uuid"),
        Some(SqlValue::Null) | None
    ));
    assert!(matches!(
        messages[1].get("is_sidechain"),
        Some(SqlValue::Integer(0))
    ));
    assert!(matches!(
        messages[2].get("id"),
        Some(SqlValue::Text(value)) if value == "claude-msg-alt"
    ));
    assert!(matches!(
        messages[2].get("parent_uuid"),
        Some(SqlValue::Text(value)) if value == "claude-msg-user"
    ));
    assert!(matches!(
        messages[2].get("is_sidechain"),
        Some(SqlValue::Integer(1))
    ));
}

#[tokio::test]
async fn test_claude_ai_empty_conversation_creates_session_and_advances_cursor() {
    let (rt, _dir) = setup().await;
    let export = serde_json::to_string(&json!([{
        "uuid": "claude-conv-empty",
        "name": "Empty Claude Conversation",
        "created_at": "2026-07-31T10:00:00Z",
        "chat_messages": []
    }]))
    .unwrap();
    let (_file, path) = write_export_file(&export);
    let file_len = std::fs::metadata(&path).unwrap().len();

    let first = mirror_claude_ai_export_file(&rt, &path, 0)
        .await
        .expect("empty claude.ai conversation ingest");
    assert_eq!(first.inserted, 0);
    assert_eq!(first.scanned, 0);
    assert_eq!(first.new_offset, file_len);
    assert_eq!(count_rows(&rt, "sessions").await, 1);
    assert_eq!(count_rows(&rt, "session_messages").await, 0);
    assert_eq!(
        cursor_offset(&rt, &path.to_string_lossy()).await,
        Some(file_len as i64)
    );

    let replay = mirror_claude_ai_export_file(&rt, &path, 0)
        .await
        .expect("empty conversation replay");
    assert_eq!(replay.inserted, 0);
    assert_eq!(count_rows(&rt, "sessions").await, 1);

    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let session = reader
        .query_row(SqlStatement {
            sql: "SELECT provider_session_id, source, slug, message_count \
                      FROM sessions WHERE id='claude-conv-empty'"
                .into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("query empty session")
        .expect("empty session row");
    assert!(matches!(
        session.get("provider_session_id"),
        Some(SqlValue::Text(value)) if value == "claude-conv-empty"
    ));
    assert!(matches!(
        session.get("source"),
        Some(SqlValue::Text(value)) if value == "claude_ai_export"
    ));
    assert!(matches!(
        session.get("slug"),
        Some(SqlValue::Text(value)) if value == "Empty Claude Conversation"
    ));
    assert!(matches!(
        session.get("message_count"),
        Some(SqlValue::Integer(0))
    ));
}

#[tokio::test]
async fn test_claude_ai_unsupported_only_conversation_creates_session() {
    let (rt, _dir) = setup().await;
    let export = serde_json::to_string(&json!([{
        "uuid": "claude-conv-internal-only",
        "summary": "Internal-only Claude Conversation",
        "updated_at": "2026-07-31T10:00:00Z",
        "chat_messages": [{
            "uuid": "claude-msg-internal-only",
            "sender": "assistant",
            "content": [
                {"type": "thinking", "thinking": "not display text"},
                {"type": "provider_internal", "payload": {"hidden": true}}
            ]
        }]
    }]))
    .unwrap();
    let (_file, path) = write_export_file(&export);
    let file_len = std::fs::metadata(&path).unwrap().len();

    let stats = mirror_claude_ai_export_file(&rt, &path, 0)
        .await
        .expect("unsupported-only claude.ai conversation ingest");
    assert_eq!(stats.inserted, 0);
    assert_eq!(stats.scanned, 0);
    assert_eq!(stats.new_offset, file_len);
    assert_eq!(count_rows(&rt, "sessions").await, 1);
    assert_eq!(count_rows(&rt, "session_messages").await, 0);
    assert_eq!(
        cursor_offset(&rt, &path.to_string_lossy()).await,
        Some(file_len as i64)
    );

    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let session = reader
        .query_row(SqlStatement {
            sql: "SELECT slug, message_count FROM sessions \
                      WHERE id='claude-conv-internal-only'"
                .into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("query unsupported-only session")
        .expect("unsupported-only session row");
    assert!(matches!(
        session.get("slug"),
        Some(SqlValue::Text(value)) if value == "Internal-only Claude Conversation"
    ));
    assert!(matches!(
        session.get("message_count"),
        Some(SqlValue::Integer(0))
    ));
}

#[tokio::test]
async fn test_claude_ai_export_secret_bearing_title_is_masked_in_stored_slug() {
    let (rt, _dir) = setup().await;
    let secret = format!("{}{}", "AKIA", "FAKEKEY1234567890");
    let export = serde_json::to_string(&json!([{
        "uuid": "claude-conv-secret-title",
        "name": format!("prod creds {secret}"),
        "created_at": "2026-07-31T10:00:00Z",
        "chat_messages": []
    }]))
    .unwrap();
    let (_file, path) = write_export_file(&export);

    mirror_claude_ai_export_file(&rt, &path, 0)
        .await
        .expect("secret-title claude.ai conversation ingest");
    assert_eq!(count_rows(&rt, "sessions").await, 1);

    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let session = reader
        .query_row(SqlStatement {
            sql: "SELECT slug FROM sessions WHERE id='claude-conv-secret-title'".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("query secret-title session")
        .expect("secret-title session row");
    let Some(SqlValue::Text(slug)) = session.get("slug") else {
        panic!("stored slug must be text");
    };
    assert!(!slug.contains(&secret));
    assert!(slug.contains("***MASKED***"));
}
