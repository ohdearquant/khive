async fn replay_row_snapshot(fixture: &Fixture, include_quarantine: bool) -> Value {
    let mut reader = fixture
        .runtime
        .sql()
        .reader()
        .await
        .expect("replay state reader");
    let mut snapshots = Vec::new();
    for sql in [
        "SELECT * FROM notes ORDER BY id",
        "SELECT * FROM attachments ORDER BY record_uuid COLLATE BINARY, role COLLATE BINARY",
    ] {
        let rows = reader
            .query_all(SqlStatement {
                sql: sql.into(),
                params: vec![],
                label: Some("keyed-replay-integrity-snapshot".into()),
            })
            .await
            .expect("replay row snapshot");
        snapshots.push(serde_json::to_value(rows).expect("snapshot native column values"));
    }
    if include_quarantine {
        let rows = reader
            .query_all(SqlStatement {
                sql: "SELECT * FROM attachment_quarantine ORDER BY record_uuid COLLATE BINARY, role COLLATE BINARY".into(),
                params: vec![], label: Some("keyed-replay-quarantine-snapshot".into()),
            })
            .await
            .expect("actual quarantine row snapshot");
        snapshots.push(serde_json::to_value(rows).expect("quarantine column values"));
    }
    json!(snapshots)
}

async fn keyed_replay_refuses_unreadable_copy(quarantined: bool) {
    for is_reply in [false, true] {
        for copy_field in ["full_id", "recipient_id"] {
            for attachment_count in [0, 2] {
                let fixture = fixture(false);
                let objects = publish(&fixture, attachment_count.max(1)).await;
                let expected = &objects[..attachment_count];
                let (actor, verb, params) = attachment_request(&fixture, is_reply, expected).await;
                let first = dispatch(&fixture, actor, verb, params.clone()).await;
                assert_copy_rows(&fixture, &first, expected).await;
                let clean_population = population(&fixture.runtime).await;
                let clean_replay = dispatch(&fixture, actor, verb, params.clone()).await;
                assert_replayed(&first, &clean_replay);
                assert_eq!(population(&fixture.runtime).await, clean_population);
                let owner = full_id(&first, copy_field);
                if quarantined {
                    let mut writer = fixture
                        .runtime
                        .sql()
                        .writer()
                        .await
                        .expect("quarantine fixture writer");
                    assert_eq!(writer.execute(SqlStatement {
                        sql: "INSERT INTO attachment_quarantine (record_uuid, substrate, role, content_ref, media_type, size_bytes, created_at, reason) VALUES (?1, 'note', ?2, ?3, NULL, ?4, 0, 'invalid_role')".into(),
                        params: vec![SqlValue::Text(owner.to_string()), SqlValue::Text("message-attachment:0\u{85}".into()),
                            SqlValue::Text(objects[0].0.as_str().to_owned()), SqlValue::Integer(objects[0].1 as i64)],
                        label: Some("keyed-replay-retained-quarantine-row".into()),
                    }).await.expect("actual quarantine schema"), 1);
                    let rows = fixture
                        .runtime
                        .attachments()
                        .unwrap()
                        .list_attachments(owner)
                        .await
                        .expect("canonical rows remain readable");
                    assert_eq!(rows.len(), attachment_count);
                } else {
                    insert_unreadable_role(
                        &fixture,
                        &[owner],
                        "message-attachment:0\u{85}",
                        &objects[0],
                    )
                    .await;
                    assert!(
                        fixture
                            .runtime
                            .attachments()
                            .unwrap()
                            .list_attachments(owner)
                            .await
                            .is_err(),
                        "the inserted row must be unreadable through the unchanged strict API"
                    );
                }
                let before = replay_row_snapshot(&fixture, quarantined).await;
                let before_population = population(&fixture.runtime).await;
                let result = fixture
                    .registry
                    .dispatch_with_identity(verb, params, Some(identity(actor)))
                    .await;
                assert!(result.is_err(), "a keyed replay must refuse an unreadable {copy_field} copy, including an attachment-free request: {result:?}");
                let error = result.unwrap_err();
                let RuntimeError::Khive(conflict) = error.refusal_source() else {
                    panic!("unreadable keyed pair must return key_conflict, got {error:?}");
                };
                assert_eq!(conflict.kind(), khive_types::ErrorKind::Conflict);
                let details = conflict.details().expect("key conflict details");
                assert_eq!(details.get("reason"), Some("key_conflict"));
                assert_eq!(details.get("existing_id"), first["full_id"].as_str());
                assert_eq!(
                    population(&fixture.runtime).await,
                    before_population,
                    "refusal must add no notes or attachments"
                );
                assert_eq!(
                    replay_row_snapshot(&fixture, quarantined).await,
                    before,
                    "refusal must preserve every note, attachment and retained quarantine value"
                );
            }
        }
    }
}

#[tokio::test]
async fn keyed_attachment_replay_refuses_unreadable_rows_on_either_copy() {
    keyed_replay_refuses_unreadable_copy(false).await;
}

#[tokio::test]
async fn keyed_attachment_replay_refuses_quarantined_rows_on_either_copy() {
    keyed_replay_refuses_unreadable_copy(true).await;
}
