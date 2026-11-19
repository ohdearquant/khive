async fn dispatch_readable_attachment_view(
    fixture: &Fixture,
    actor: &str,
    verb: &str,
    params: Value,
) -> Value {
    let result = fixture
        .registry
        .dispatch_with_identity(verb, params, Some(identity(actor)))
        .await;
    assert!(
        result.is_ok(),
        "unreadable row must not fail {verb}: {result:?}"
    );
    result.unwrap()
}

fn assert_attachment_error(message: &Value, count: u64) {
    assert_eq!(
        message.get("attachments_error"),
        Some(&json!({"count": count, "reason": "unreadable_attachment"})),
        "unreadable attachment rows must be reported at their message"
    );
}

fn view_message<'a>(messages: &'a [Value], id: &str) -> &'a Value {
    let matching: Vec<_> = messages
        .iter()
        .filter(|message| message["full_id"] == id)
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "the expected message must appear exactly once"
    );
    matching[0]
}

async fn stored_message_read_flag(fixture: &Fixture, id: &str) -> SqlValue {
    let value = fixture.runtime.sql().reader().await.expect("read-flag reader")
        .query_scalar(SqlStatement {
            sql: "SELECT coalesce(json_extract(properties, '$.read'), 'absent') FROM notes WHERE id = ?1".into(),
            params: vec![SqlValue::Text(id.to_owned())],
            label: Some("attachment-fixture-stored-read-flag".into()),
        }).await.expect("read the flag independently of attachment enrichment");
    value.expect("the real inbound note exists")
}

async fn insert_unreadable_role(
    fixture: &Fixture,
    ids: &[Uuid],
    role: &str,
    object: &(ContentRef, u64),
) {
    let mut writer = fixture
        .runtime
        .sql()
        .writer()
        .await
        .expect("legacy-row fixture writer");
    writer
        .execute(SqlStatement {
            sql: "PRAGMA ignore_check_constraints = ON".into(),
            params: vec![],
            label: Some("attachment-fixture-enable-legacy-row".into()),
        })
        .await
        .expect("bypass role CHECK only on this private fixture writer");
    assert!(matches!(
        writer
            .query_scalar(SqlStatement {
                sql: "PRAGMA ignore_check_constraints".into(),
                params: vec![],
                label: None,
            })
            .await
            .unwrap(),
        Some(SqlValue::Integer(1))
    ));
    let inserted = writer.execute_batch(ids.iter().map(|id| SqlStatement {
        sql: "INSERT INTO attachments (record_uuid, substrate, role, content_ref, media_type, size_bytes, created_at) VALUES (?1, 'note', ?2, ?3, NULL, ?4, 0)".into(),
        params: vec![SqlValue::Text(id.to_string()), SqlValue::Text(role.into()),
            SqlValue::Text(object.0.as_str().to_owned()), SqlValue::Integer(object.1 as i64)],
        label: Some("attachment-fixture-legacy-row".into()),
    }).collect()).await;
    writer
        .execute(SqlStatement {
            sql: "PRAGMA ignore_check_constraints = OFF".into(),
            params: vec![],
            label: Some("attachment-fixture-restore-role-check".into()),
        })
        .await
        .expect("restore role CHECK before releasing the writer");
    assert!(matches!(
        writer
            .query_scalar(SqlStatement {
                sql: "PRAGMA ignore_check_constraints".into(),
                params: vec![],
                label: None,
            })
            .await
            .unwrap(),
        Some(SqlValue::Integer(0))
    ));
    assert_eq!(
        inserted.expect("seed the actual unreadable row"),
        ids.len() as u64
    );
}

async fn readable_thread_pair(fixture: &Fixture, objects: &[(ContentRef, u64)]) -> (Value, Value) {
    let first = dispatch(fixture, SENDER, "comm.send", json!({
        "to": RECIPIENT, "content": "attachment row owner", "attachments": [objects[0].0.as_str()],
        "idempotency_key": "row-owner",
    })).await;
    let second = dispatch(
        fixture,
        SENDER,
        "comm.send",
        json!({
            "to": RECIPIENT, "content": "readable sibling", "attachments": [objects[1].0.as_str()],
            "thread_id": first["thread_id"], "idempotency_key": "row-sibling",
        }),
    )
    .await;
    assert_eq!(first["thread_id"], second["thread_id"]);
    assert_copy_rows(fixture, &first, &objects[..1]).await;
    assert_copy_rows(fixture, &second, &objects[1..2]).await;
    let guard = inbox(fixture, RECIPIENT, "inbox").await;
    let messages = guard["messages"].as_array().unwrap();
    assert_eq!(
        messages.len(),
        2,
        "both actual inbound siblings exist before corruption"
    );
    assert_metadata(
        view_message(messages, first["recipient_id"].as_str().unwrap()),
        &objects[..1],
    );
    assert_metadata(
        view_message(messages, second["recipient_id"].as_str().unwrap()),
        &objects[1..2],
    );
    assert!(messages
        .iter()
        .all(|message| message.get("attachments_error").is_none()));
    (first, second)
}

async fn unreadable_role_preserves_message_views(role: &str) {
    let fixture = fixture(false);
    let objects = publish(&fixture, 2).await;
    let (first, second) = readable_thread_pair(&fixture, &objects).await;
    insert_unreadable_role(
        &fixture,
        &[full_id(&first, "full_id"), full_id(&first, "recipient_id")],
        role,
        &objects[0],
    )
    .await;
    for (actor, box_name, id_field) in [
        (RECIPIENT, "inbox", "recipient_id"),
        (SENDER, "sent", "full_id"),
    ] {
        let view = dispatch_readable_attachment_view(
            &fixture,
            actor,
            "comm.inbox",
            mailbox_params(box_name),
        )
        .await;
        let messages = view["messages"].as_array().unwrap();
        assert_eq!(
            messages.len(),
            2,
            "one unreadable row cannot hide either message"
        );
        let affected = view_message(messages, first[id_field].as_str().unwrap());
        assert_metadata(affected, &objects[..1]);
        assert_attachment_error(affected, 1);
        let clean = view_message(messages, second[id_field].as_str().unwrap());
        assert_metadata(clean, &objects[1..2]);
        assert!(clean.get("attachments_error").is_none());
    }
    let projected = dispatch_readable_attachment_view(
        &fixture,
        RECIPIENT,
        "comm.inbox",
        json!({"box": "inbox", "status": "all",
            "fields": ["full_id", "attachments", "attachments_error"]}),
    )
    .await;
    let projected_messages = projected["messages"].as_array().unwrap();
    assert_eq!(projected_messages.len(), 2);
    let affected = view_message(projected_messages, first["recipient_id"].as_str().unwrap());
    assert_eq!(affected.as_object().unwrap().len(), 3);
    assert_metadata(affected, &objects[..1]);
    assert_attachment_error(affected, 1);
    let clean = view_message(projected_messages, second["recipient_id"].as_str().unwrap());
    assert_metadata(clean, &objects[1..2]);
    assert_eq!(clean.as_object().unwrap().len(), 3);
    assert_eq!(
        clean.get("attachments_error"),
        Some(&Value::Null),
        "a requested absent field projects as null; the clean message must not carry an error marker"
    );
    let thread = dispatch_readable_attachment_view(
        &fixture,
        RECIPIENT,
        "comm.thread",
        json!({"id": first["recipient_id"]}),
    )
    .await;
    let messages = thread["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_metadata(
        view_message(messages, first["full_id"].as_str().unwrap()),
        &objects[..1],
    );
    assert_attachment_error(
        view_message(messages, first["full_id"].as_str().unwrap()),
        1,
    );
    assert_metadata(
        view_message(messages, second["full_id"].as_str().unwrap()),
        &objects[1..2],
    );
    assert!(view_message(messages, second["full_id"].as_str().unwrap())
        .get("attachments_error")
        .is_none());
    let read = dispatch_readable_attachment_view(
        &fixture,
        RECIPIENT,
        "comm.read",
        json!({"id": first["recipient_id"], "body": true}),
    )
    .await;
    assert_eq!(read["status"], "success");
    assert_eq!(read["full_id"], first["recipient_id"]);
    assert_metadata(&read, &objects[..1]);
    assert_attachment_error(&read, 1);
    assert!(matches!(
        stored_message_read_flag(&fixture, first["recipient_id"].as_str().unwrap()).await,
        SqlValue::Integer(1)
    ));
}

#[tokio::test]
async fn unreadable_c0_attachment_rows_keep_message_views_readable() {
    unreadable_role_preserves_message_views("message-attachment:0\u{1}").await;
}

#[tokio::test]
async fn unreadable_c1_attachment_rows_keep_message_views_readable() {
    unreadable_role_preserves_message_views("message-attachment:0\u{85}").await;
}

#[tokio::test]
async fn bulk_read_with_unreadable_later_row_marks_both_messages() {
    let fixture = fixture(false);
    let first = dispatch(
        &fixture,
        SENDER,
        "comm.send",
        json!({
            "to": RECIPIENT, "content": "first", "idempotency_key": "bulk-first",
        }),
    )
    .await;
    let second = dispatch(
        &fixture,
        SENDER,
        "comm.send",
        json!({
            "to": RECIPIENT, "content": "second", "idempotency_key": "bulk-second",
        }),
    )
    .await;
    let objects = publish(&fixture, 1).await;
    let first_id = first["recipient_id"].as_str().unwrap();
    let second_id = second["recipient_id"].as_str().unwrap();
    insert_unreadable_role(
        &fixture,
        &[full_id(&second, "recipient_id")],
        "message-attachment:0\u{1}",
        &objects[0],
    )
    .await;
    assert!(matches!(
        stored_message_read_flag(&fixture, first_id).await,
        SqlValue::Integer(0)
    ));
    assert!(matches!(
        stored_message_read_flag(&fixture, second_id).await,
        SqlValue::Integer(0)
    ));
    let result = fixture
        .registry
        .dispatch_with_identity(
            "comm.read",
            json!({"ids": [first_id, second_id], "body": true}),
            Some(identity(RECIPIENT)),
        )
        .await;
    assert!(
        result.is_ok(),
        "unreadable row must not fail bulk comm.read: {result:?}"
    );
    let result = result.unwrap();
    assert_eq!(result["status"], "success");
    assert_eq!(result["requested_count"], 2);
    assert_eq!(result["unique_count"], 2);
    assert_eq!(result["marked_count"], 2);
    assert_eq!(result["failed_count"], 0);
    assert_eq!(result["unknown_count"], 0);
    let messages = result["results"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    for (message, id) in messages.iter().zip([first_id, second_id]) {
        assert_eq!(message["full_id"], id);
        assert_eq!(message["status"], "success");
        assert_eq!(message["read"], true);
        assert_eq!(message["properties"]["read"], true);
        assert_metadata(message, &[]);
        assert!(matches!(
            stored_message_read_flag(&fixture, id).await,
            SqlValue::Integer(1)
        ));
    }
    assert!(messages[0].get("attachments_error").is_none());
    assert_attachment_error(&messages[1], 1);
}
