use super::*;

async fn attached_root(fixture: &Fixture, objects: &[(ContentRef, u64)]) -> Value {
    dispatch(
        fixture,
        SENDER,
        "comm.send",
        json!({
            "to": RECIPIENT, "content": "attached root", "attachments": refs(objects),
            "idempotency_key": "lifecycle-root",
        }),
    )
    .await
}

async fn note_deleted_at(fixture: &Fixture, id: Uuid) -> Option<SqlValue> {
    fixture
        .runtime
        .sql()
        .reader()
        .await
        .expect("note state reader")
        .query_scalar(SqlStatement {
            sql: "SELECT deleted_at FROM notes WHERE id = ?1".into(),
            params: vec![SqlValue::Text(id.to_string())],
            label: Some("attachment-lifecycle-note-state".into()),
        })
        .await
        .expect("persisted note state")
}

#[tokio::test]
async fn attached_root_and_reply_keep_distinct_metadata_in_both_thread_orders() {
    let fixture = fixture(false);
    let objects = publish(&fixture, 3).await;
    let root = attached_root(&fixture, &objects[..2]).await;
    let reply = dispatch(
        &fixture,
        RECIPIENT,
        "comm.reply",
        json!({
            "id": root["recipient_id"], "content": "attached reply",
            "attachments": refs(&objects[2..]), "idempotency_key": "lifecycle-reply",
        }),
    )
    .await;
    assert_eq!(reply["thread_id"], root["thread_id"]);
    assert_copy_rows(&fixture, &root, &objects[..2]).await;
    assert_copy_rows(&fixture, &reply, &objects[2..]).await;
    assert_eq!(count(&fixture.runtime, "attachments").await, 6);
    for actor in [SENDER, RECIPIENT] {
        for order in ["asc", "desc"] {
            let thread = dispatch(
                &fixture,
                actor,
                "comm.thread",
                json!({"id": root["thread_id"], "order": order}),
            )
            .await;
            let messages = thread["messages"].as_array().unwrap();
            assert_eq!(messages.len(), 2, "dual copies appear once per message");
            let expected = if order == "asc" {
                [(&root, &objects[..2]), (&reply, &objects[2..])]
            } else {
                [(&reply, &objects[2..]), (&root, &objects[..2])]
            };
            for (message, (receipt, files)) in messages.iter().zip(expected) {
                assert_eq!(message["full_id"], receipt["full_id"]);
                assert_metadata(message, files);
                assert!(message.get("attachments_error").is_none());
            }
        }
    }
}

#[tokio::test]
async fn attachment_free_reply_does_not_inherit_the_parents_files() {
    let fixture = fixture(false);
    let objects = publish(&fixture, 2).await;
    let root = attached_root(&fixture, &objects).await;
    let reply = dispatch(
        &fixture,
        RECIPIENT,
        "comm.reply",
        json!({
            "id": root["recipient_id"], "content": "plain reply",
            "idempotency_key": "plain-lifecycle-reply",
        }),
    )
    .await;
    assert_copy_rows(&fixture, &root, &objects).await;
    assert_copy_rows(&fixture, &reply, &[]).await;
    assert_eq!(
        population(&fixture.runtime).await,
        Population {
            notes: 4,
            attachments: 4
        }
    );
    for order in ["asc", "desc"] {
        let thread = dispatch(
            &fixture,
            RECIPIENT,
            "comm.thread",
            json!({"id": root["thread_id"], "order": order}),
        )
        .await;
        let messages = thread["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_metadata(
            view_message(messages, root["full_id"].as_str().unwrap()),
            &objects,
        );
        assert_metadata(
            view_message(messages, reply["full_id"].as_str().unwrap()),
            &[],
        );
    }
}

#[tokio::test]
async fn soft_delete_keeps_each_copys_attachment_rows_and_bytes() {
    for target_field in ["full_id", "recipient_id"] {
        let fixture = fixture(false);
        let objects = publish(&fixture, 2).await;
        let root = attached_root(&fixture, &objects).await;
        let target = full_id(&root, target_field);
        let other = full_id(
            &root,
            if target_field == "full_id" {
                "recipient_id"
            } else {
                "full_id"
            },
        );
        assert!(matches!(
            note_deleted_at(&fixture, target).await,
            Some(SqlValue::Null)
        ));
        let deleted = dispatch(
            &fixture,
            SENDER,
            "delete",
            json!({"id": target, "hard": false}),
        )
        .await;
        assert_eq!(deleted["deleted"], true);
        assert!(matches!(
            note_deleted_at(&fixture, target).await,
            Some(SqlValue::Integer(_))
        ));
        assert!(matches!(
            note_deleted_at(&fixture, other).await,
            Some(SqlValue::Null)
        ));
        assert_copy_rows(&fixture, &root, &objects).await;
        assert_eq!(
            population(&fixture.runtime).await,
            Population {
                notes: 2,
                attachments: 4
            }
        );
        for (index, (reference, size)) in objects.iter().enumerate() {
            assert_eq!(fixture.blobs.size(reference).await.unwrap(), Some(*size));
            assert_eq!(
                fixture
                    .blobs
                    .get_bounded_verified(reference, 64)
                    .await
                    .unwrap(),
                vec![b'a' + index as u8; *size as usize]
            );
        }
    }
}

#[tokio::test]
async fn hard_delete_releases_only_the_target_copys_attachment_rows() {
    for target_field in ["full_id", "recipient_id"] {
        let fixture = fixture(false);
        let objects = publish(&fixture, 2).await;
        let root = attached_root(&fixture, &objects).await;
        let target = full_id(&root, target_field);
        let other = full_id(
            &root,
            if target_field == "full_id" {
                "recipient_id"
            } else {
                "full_id"
            },
        );
        let store = fixture.runtime.attachments().unwrap();
        let survivor_before = store.list_attachments(other).await.unwrap();
        assert_eq!(survivor_before.len(), 2);
        let deleted = dispatch(
            &fixture,
            SENDER,
            "delete",
            json!({"id": target, "hard": true}),
        )
        .await;
        assert_eq!(deleted["deleted"], true);
        assert!(note_deleted_at(&fixture, target).await.is_none());
        assert!(matches!(
            note_deleted_at(&fixture, other).await,
            Some(SqlValue::Null)
        ));
        assert!(store.list_attachments(target).await.unwrap().is_empty());
        assert_eq!(
            store.list_attachments(other).await.unwrap(),
            survivor_before
        );
        assert_eq!(
            population(&fixture.runtime).await,
            Population {
                notes: 1,
                attachments: 2
            }
        );
        for (index, (reference, size)) in objects.iter().enumerate() {
            assert!(fixture.blobs.exists(reference).await.unwrap());
            assert_eq!(
                fixture
                    .blobs
                    .get_bounded_verified(reference, 64)
                    .await
                    .unwrap(),
                vec![b'a' + index as u8; *size as usize]
            );
        }
        let actor = if target_field == "full_id" {
            RECIPIENT
        } else {
            SENDER
        };
        let box_name = if target_field == "full_id" {
            "inbox"
        } else {
            "sent"
        };
        let view = inbox(&fixture, actor, box_name).await;
        let messages = view["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_metadata(view_message(messages, &other.to_string()), &objects);
    }
}

#[tokio::test]
async fn hard_delete_attachment_failure_rolls_back_the_note_and_both_copies_rows() {
    let fixture = fixture(false);
    let objects = publish(&fixture, 2).await;
    let root = attached_root(&fixture, &objects).await;
    let target = full_id(&root, "full_id");
    fixture.runtime.sql().writer().await.unwrap().execute(SqlStatement {
        sql: format!("CREATE TRIGGER refuse_attachment_delete BEFORE DELETE ON attachments WHEN OLD.record_uuid = '{target}' BEGIN SELECT RAISE(ABORT, 'attachment-delete-control'); END"),
        params: vec![], label: Some("attachment-delete-fixture-fault".into()),
    }).await.expect("install a private attachment-deletion fault");
    let result = fixture
        .registry
        .dispatch_with_identity(
            "delete",
            json!({"id": target, "hard": true}),
            Some(identity(SENDER)),
        )
        .await;
    let error =
        result.expect_err("attachment cleanup failure rolls back the hard-delete transaction");
    assert!(
        error.to_string().contains("attachment-delete-control"),
        "the injected attachment failure must be reached: {error}"
    );
    for field in ["full_id", "recipient_id"] {
        assert!(matches!(
            note_deleted_at(&fixture, full_id(&root, field)).await,
            Some(SqlValue::Null)
        ));
    }
    assert_copy_rows(&fixture, &root, &objects).await;
    assert_eq!(
        population(&fixture.runtime).await,
        Population {
            notes: 2,
            attachments: 4
        }
    );
}

#[tokio::test]
async fn non_message_roles_contribute_only_unreadable_owner_diagnostics() {
    let fixture = fixture(false);
    let objects = publish(&fixture, 2).await;
    let (affected, clean) = readable_thread_pair(&fixture, &objects).await;
    for field in ["full_id", "recipient_id"] {
        let id = full_id(&affected, field);
        fixture
            .runtime
            .attachments()
            .unwrap()
            .upsert_attachment(Attachment::from_new(
                id,
                AttachmentSubstrate::Note,
                NewAttachment {
                    role: "content".into(),
                    content_ref: objects[1].0.clone(),
                    media_type: None,
                    size_bytes: Some(objects[1].1),
                },
                0,
            ))
            .await
            .unwrap();
        let mut writer = fixture.runtime.sql().writer().await.unwrap();
        writer
            .execute(SqlStatement {
                sql: "PRAGMA ignore_check_constraints = ON".into(),
                params: vec![],
                label: None,
            })
            .await
            .unwrap();
        let inserted = writer.execute(SqlStatement {
            sql: "INSERT INTO attachments (record_uuid, substrate, role, content_ref, media_type, size_bytes, created_at) VALUES (?1, 'note', 'quarantine-original', 'invalid-ref', NULL, 7, 0)".into(),
            params: vec![SqlValue::Text(id.to_string())], label: Some("non-message-unreadable-fixture".into()),
        }).await;
        writer
            .execute(SqlStatement {
                sql: "PRAGMA ignore_check_constraints = OFF".into(),
                params: vec![],
                label: None,
            })
            .await
            .unwrap();
        assert_eq!(inserted.unwrap(), 1);
        writer.execute(SqlStatement {
            sql: "INSERT INTO attachment_quarantine (record_uuid, substrate, role, content_ref, media_type, size_bytes, created_at, reason) VALUES (?1, 'note', ?2, ?3, NULL, 7, 0, 'invalid_role')".into(),
            params: vec![SqlValue::Text(id.to_string()), SqlValue::Text("quarantine-original\u{85}".into()), SqlValue::Text(objects[1].0.to_string())], label: Some("non-message-quarantine-fixture".into()),
        }).await.unwrap();
    }
    for (actor, box_name, field) in [
        (SENDER, "sent", "full_id"),
        (RECIPIENT, "inbox", "recipient_id"),
    ] {
        let view = inbox(&fixture, actor, box_name).await;
        let messages = view["messages"].as_array().unwrap();
        assert_metadata(
            view_message(messages, affected[field].as_str().unwrap()),
            &objects[..1],
        );
        assert_attachment_error(view_message(messages, affected[field].as_str().unwrap()), 2);
        let clean_message = view_message(messages, clean[field].as_str().unwrap());
        assert_metadata(clean_message, &objects[1..]);
        assert!(clean_message.get("attachments_error").is_none());
    }
    for order in ["asc", "desc"] {
        let thread = dispatch(
            &fixture,
            RECIPIENT,
            "comm.thread",
            json!({"id": affected["thread_id"], "order": order}),
        )
        .await;
        let messages = thread["messages"].as_array().unwrap();
        assert_metadata(
            view_message(messages, affected["full_id"].as_str().unwrap()),
            &objects[..1],
        );
        assert_attachment_error(
            view_message(messages, affected["full_id"].as_str().unwrap()),
            2,
        );
        assert!(view_message(messages, clean["full_id"].as_str().unwrap())
            .get("attachments_error")
            .is_none());
    }
    let projected = dispatch(
        &fixture,
        RECIPIENT,
        "comm.inbox",
        json!({"status": "all", "fields": ["full_id", "attachments_error"]}),
    )
    .await;
    let messages = projected["messages"].as_array().unwrap();
    let affected_projection = view_message(messages, affected["recipient_id"].as_str().unwrap());
    assert_eq!(affected_projection.as_object().unwrap().len(), 2);
    assert_attachment_error(affected_projection, 2);
    let clean_projection = view_message(messages, clean["recipient_id"].as_str().unwrap());
    assert_eq!(clean_projection.as_object().unwrap().len(), 2);
    assert_eq!(
        clean_projection.get("attachments_error"),
        Some(&Value::Null)
    );
    let ack = dispatch(
        &fixture,
        RECIPIENT,
        "comm.read",
        json!({"id": affected["recipient_id"], "body": false}),
    )
    .await;
    assert_acknowledgement_only(&ack);
    assert!(ack.get("attachments_error").is_none());
    let read = dispatch(
        &fixture,
        RECIPIENT,
        "comm.read",
        json!({"id": affected["recipient_id"], "body": true}),
    )
    .await;
    assert_eq!(read["status"], "success");
    assert_metadata(&read, &objects[..1]);
    assert_attachment_error(&read, 2);
}
