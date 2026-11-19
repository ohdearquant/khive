#[tokio::test]
async fn test_chatgpt_secret_bearing_conversations_json_is_masked() {
    // Assembled from fragments at runtime so no credential-shaped literal
    // is committed to the repo; matches the AWS-key shape already covered
    // by `khive_runtime::secret_gate`'s own detector tests.
    let secret_fragment_a = "AKIA";
    let secret_fragment_b = "FAKEKEY1234567890";
    let secret = format!("{secret_fragment_a}{secret_fragment_b}");
    let user_text = format!("here is my key: {secret}");

    let conv = json!({
        "id": "conv-secret",
        "title": "Synthetic Secret",
        "current_node": "msg-secret-user",
        "mapping": {
            "root-secret": {
                "id": "root-secret",
                "message": null,
                "parent": null,
                "children": ["msg-secret-user"]
            },
            "msg-secret-user": {
                "id": "msg-secret-user",
                "parent": "root-secret",
                "children": [],
                "message": {
                    "id": "msg-secret-user",
                    "author": {"role": "user"},
                    "content": {"content_type": "text", "parts": [user_text]}
                }
            }
        }
    });
    let content = serde_json::to_string(&json!([conv])).unwrap();
    let (_file, path) = write_export_file(&content);

    let (rt, _dir) = setup().await;
    let stats = mirror_chatgpt_export_file(&rt, &path, 0)
        .await
        .expect("secret-bearing content must still ingest, only masked");
    assert_eq!(stats.inserted, 1);

    let sql = rt.sql();
    let mut r = sql.reader().await.expect("reader");
    let row = r
        .query_row(SqlStatement {
            sql: "SELECT text, raw FROM session_messages WHERE session_id='conv-secret'".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("query ok")
        .expect("message row must exist");
    let (stored_text, stored_raw) = match (row.get("text"), row.get("raw")) {
        (Some(SqlValue::Text(t)), Some(SqlValue::Text(r))) => (t.clone(), r.clone()),
        other => panic!("unexpected text/raw shape: {other:?}"),
    };

    assert!(
        !stored_text.contains(&secret),
        "stored text must not contain the raw secret"
    );
    assert!(
        !stored_raw.contains(&secret),
        "stored raw must not contain the raw secret"
    );
    assert!(
        stored_text.contains("***MASKED***"),
        "stored text must carry the secret_gate redaction marker"
    );
    assert!(
        stored_raw.contains("***MASKED***"),
        "stored raw must carry the secret_gate redaction marker"
    );
}

#[tokio::test]
async fn test_chatgpt_export_secret_bearing_title_is_masked_in_stored_slug() {
    let (rt, _dir) = setup().await;
    let secret = format!("{}{}", "AKIA", "FAKEKEY1234567890");
    let conv = json!({
        "id": "conv-secret-title",
        "title": format!("prod creds {secret}"),
        "current_node": "msg-secret-title-user",
        "mapping": {
            "root-secret-title": {
                "id": "root-secret-title",
                "message": null,
                "parent": null,
                "children": ["msg-secret-title-user"]
            },
            "msg-secret-title-user": {
                "id": "msg-secret-title-user",
                "parent": "root-secret-title",
                "children": [],
                "message": {
                    "id": "msg-secret-title-user",
                    "author": {"role": "user"},
                    "content": {"content_type": "text", "parts": ["Hello"]}
                }
            }
        }
    });
    let content = serde_json::to_string(&json!([conv])).unwrap();
    let (_file, path) = write_export_file(&content);

    mirror_chatgpt_export_file(&rt, &path, 0)
        .await
        .expect("secret-title chatgpt conversation ingest");
    assert_eq!(count_rows(&rt, "sessions").await, 1);

    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let session = reader
        .query_row(SqlStatement {
            sql: "SELECT slug FROM sessions WHERE id='conv-secret-title'".into(),
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

    // The pre-mask title must not be recoverable from any other stored
    // column either — the message-bearing node this conversation carries
    // has no title field of its own, so text/raw must not echo it back.
    let message_row = reader
        .query_row(SqlStatement {
            sql: "SELECT text, raw FROM session_messages WHERE session_id='conv-secret-title'"
                .into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("query secret-title message")
        .expect("secret-title message row");
    let (Some(SqlValue::Text(stored_text)), Some(SqlValue::Text(stored_raw))) =
        (message_row.get("text"), message_row.get("raw"))
    else {
        panic!("stored text/raw must be text");
    };
    assert!(!stored_text.contains(&secret));
    assert!(!stored_raw.contains(&secret));
}
