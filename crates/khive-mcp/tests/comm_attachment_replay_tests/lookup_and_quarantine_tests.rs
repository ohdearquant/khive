async fn install_later_target_sql_failure(fixture: &Fixture, id: Uuid) {
    let mut writer = fixture
        .runtime
        .sql()
        .writer()
        .await
        .expect("private view writer");
    writer
        .execute_script(format!(
            "ALTER TABLE attachments RENAME TO fixture_attachment_rows; \
         CREATE VIEW attachments AS \
         SELECT record_uuid, substrate, role, \
             json_extract(CASE WHEN record_uuid = '{}' THEN 'not-json' \
                 ELSE '\"' || content_ref || '\"' END, '$') AS content_ref, \
             media_type, size_bytes, created_at FROM fixture_attachment_rows;",
            id
        ))
        .await
        .expect("install target-specific SQL execution failure");
}

#[tokio::test]
async fn bulk_attachment_lookup_failure_returns_before_any_mark() {
    let fixture = fixture(false);
    let objects = publish(&fixture, 2).await;
    let (first, second) = readable_thread_pair(&fixture, &objects).await;
    let first_id = first["recipient_id"].as_str().unwrap();
    let second_id = second["recipient_id"].as_str().unwrap();
    install_later_target_sql_failure(&fixture, full_id(&second, "recipient_id")).await;
    let store = fixture.runtime.attachments().unwrap();
    let first_rows = store
        .list_attachments(full_id(&first, "recipient_id"))
        .await
        .expect("the first actual attachment lookup still succeeds");
    assert_eq!(first_rows.len(), 1);
    assert_eq!(first_rows[0].content_ref, objects[0].0);
    let cursor_error = fixture.runtime.sql().reader().await.unwrap().query_all(SqlStatement {
        sql: "SELECT record_uuid, substrate, role, content_ref, media_type, size_bytes, created_at FROM attachments WHERE record_uuid = ?1 ORDER BY role ASC".into(),
        params: vec![SqlValue::Text(second_id.to_owned())],
        label: Some("attachment-fixture-step-failure".into()),
    }).await.expect_err("SQL cursor stepping must fail before attachment row decoding");
    assert!(matches!(
        &cursor_error,
        khive_storage::StorageError::Driver { .. }
    ));
    assert!(
        cursor_error.to_string().contains("malformed JSON"),
        "{cursor_error}"
    );
    assert!(matches!(
        stored_message_read_flag(&fixture, first_id).await,
        SqlValue::Integer(0)
    ));
    assert!(matches!(
        stored_message_read_flag(&fixture, second_id).await,
        SqlValue::Integer(0)
    ));
    let error = fixture
        .registry
        .dispatch_with_identity(
            "comm.read",
            json!({"ids": [first_id, second_id], "body": true}),
            Some(identity(RECIPIENT)),
        )
        .await
        .expect_err("a whole later-target lookup failure must refuse the bulk read");
    assert!(matches!(
        &error,
        RuntimeError::Storage(khive_storage::StorageError::Driver { .. })
    ));
    assert!(error.to_string().contains("malformed JSON"), "{error}");
    assert!(
        matches!(
            stored_message_read_flag(&fixture, first_id).await,
            SqlValue::Integer(0)
        ),
        "all field reads must complete before the first message is marked"
    );
    assert!(matches!(
        stored_message_read_flag(&fixture, second_id).await,
        SqlValue::Integer(0)
    ));
    let ack = dispatch_readable_attachment_view(
        &fixture,
        RECIPIENT,
        "comm.read",
        json!({"ids": [first_id, second_id], "body": false}),
    )
    .await;
    assert_eq!(ack["status"], "success");
    assert_eq!(
        ack["marked_count"], 2,
        "body=false does not query the failing attachment view"
    );
    for message in ack["results"].as_array().unwrap() {
        assert_acknowledgement_only(message);
    }
    assert!(matches!(
        stored_message_read_flag(&fixture, first_id).await,
        SqlValue::Integer(1)
    ));
    assert!(matches!(
        stored_message_read_flag(&fixture, second_id).await,
        SqlValue::Integer(1)
    ));
}

#[tokio::test]
async fn quarantined_attachment_rows_remain_visible_in_message_error_counts() {
    let fixture = fixture(false);
    let objects = publish(&fixture, 2).await;
    let (first, second) = readable_thread_pair(&fixture, &objects).await;
    let mut writer = fixture
        .runtime
        .sql()
        .writer()
        .await
        .expect("quarantine fixture writer");
    let changed = writer.execute_batch(["full_id", "recipient_id"].iter().map(|field| SqlStatement {
        sql: "INSERT INTO attachment_quarantine (record_uuid, substrate, role, content_ref, media_type, size_bytes, created_at, reason) VALUES (?1, 'note', ?2, ?3, NULL, ?4, 0, 'invalid_role')".into(),
        params: vec![SqlValue::Text(full_id(&first, field).to_string()),
            SqlValue::Text("message-attachment:0\u{1}".into()),
            SqlValue::Text(objects[0].0.as_str().to_owned()), SqlValue::Integer(objects[0].1 as i64)],
        label: Some("attachment-fixture-quarantined-role".into()),
    }).collect()).await.expect("candidate migration 047 provides the actual quarantine schema");
    assert_eq!(changed, 2);
    drop(writer);
    assert_copy_rows(&fixture, &first, &objects[..1]).await;
    for field in ["full_id", "recipient_id"] {
        let count = fixture
            .runtime
            .sql()
            .reader()
            .await
            .unwrap()
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM attachment_quarantine WHERE record_uuid = ?1".into(),
                params: vec![SqlValue::Text(full_id(&first, field).to_string())],
                label: Some("attachment-fixture-quarantine-count".into()),
            })
            .await
            .unwrap();
        assert!(matches!(count, Some(SqlValue::Integer(1))));
    }
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
        assert_eq!(messages.len(), 2);
        let affected = view_message(messages, first[id_field].as_str().unwrap());
        assert_metadata(affected, &objects[..1]);
        assert_attachment_error(affected, 1);
        let clean = view_message(messages, second[id_field].as_str().unwrap());
        assert_metadata(clean, &objects[1..2]);
        assert!(clean.get("attachments_error").is_none());
    }
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
    assert_metadata(&read, &objects[..1]);
    assert_attachment_error(&read, 1);
    assert!(matches!(
        stored_message_read_flag(&fixture, first["recipient_id"].as_str().unwrap()).await,
        SqlValue::Integer(1)
    ));
}
