async fn attachment_replay_publication_snapshot(fixture: &Fixture) -> Value {
    let rows = replay_row_snapshot(fixture, false).await;
    let mut reader = fixture
        .runtime
        .sql()
        .reader()
        .await
        .expect("replay event snapshot reader");
    let mut events = Vec::new();
    for sql in [
        "SELECT * FROM events ORDER BY id",
        "SELECT * FROM event_observations ORDER BY event_id, role, position",
    ] {
        events.push(
            serde_json::to_value(
                reader
                    .query_all(SqlStatement {
                        sql: sql.into(),
                        params: vec![],
                        label: Some("attachment-replay-publication-snapshot".into()),
                    })
                    .await
                    .expect("replay event snapshot query"),
            )
            .expect("snapshot SQL column values"),
        );
    }
    json!({"notes_and_attachments": rows, "events_and_observations": events})
}

#[tokio::test]
async fn attachment_free_key_replays_pre_attachment_request_shape() {
    let fixture = fixture(false);
    let params = json!({
        "to": RECIPIENT, "content": "message from before file attachments",
        "subject": "Legacy key", "tags": ["old-shape"], "idempotency_key": "legacy-empty-files",
    });
    let first = dispatch(&fixture, SENDER, "comm.send", params.clone()).await;
    assert_eq!(
        population(&fixture.runtime).await,
        Population {
            notes: 2,
            attachments: 0
        }
    );
    assert_copy_rows(&fixture, &first, &[]).await;
    // Install the literal historical request on a real, intact public-send
    // pair. This stays independent of today's identity constructor, including
    // if it starts adding an empty attachments member to newly minted keys.
    let historical = json!({
        "version": 1, "op": "send", "to": RECIPIENT,
        "content": "message from before file attachments", "subject": "Legacy key",
        "thread_id": null, "tags": ["old-shape"], "reply_parent_id": null,
    });
    assert!(historical.get("attachments").is_none());
    let outbound = full_id(&first, "full_id");
    let changed = fixture.runtime.sql().writer().await.expect("historical identity writer")
        .execute(SqlStatement {
            sql: "UPDATE notes SET properties=json_set(properties, '$.idempotency_request', json(?1)) \
                  WHERE id=?2 AND namespace=?3 AND kind='message' AND key IS NOT NULL".into(),
            params: vec![SqlValue::Text(historical.to_string()), SqlValue::Text(outbound.to_string()), SqlValue::Text(NAMESPACE.into())],
            label: Some("install-pre-attachment-message-request".into()),
        }).await.expect("store the pre-attachment request shape");
    assert_eq!(changed, 1, "only the held outbound identity is installed");
    let token = fixture.runtime.authorize(Namespace::local()).unwrap();
    let stored = fixture
        .runtime
        .notes(&token)
        .unwrap()
        .get_note(outbound)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.properties.as_ref().unwrap()["idempotency_request"],
        historical
    );
    let physical_key = format!(
        "comm-v1:{}",
        json!([NAMESPACE, SENDER, "legacy-empty-files"])
    );
    assert_eq!(stored.key.as_deref(), Some(physical_key.as_str()));
    let before = attachment_replay_publication_snapshot(&fixture).await;
    for explicit_empty in [false, true] {
        let mut replay_params = params.clone();
        if explicit_empty {
            replay_params["attachments"] = json!([]);
        }
        let result = fixture
            .registry
            .dispatch_with_identity("comm.send", replay_params, Some(identity(SENDER)))
            .await;
        assert!(
            result.is_ok(),
            "omitted and empty files must replay the stored historical identity: {result:?}"
        );
        let replay = result.unwrap();
        assert_replayed(&first, &replay);
        assert_copy_rows(&fixture, &replay, &[]).await;
        assert_eq!(attachment_replay_publication_snapshot(&fixture).await, before,
            "legacy replay must not modify either note, its key, attachments, events or projections");
    }
}

#[tokio::test]
async fn keyed_attachment_replay_refuses_different_list_and_order() {
    for is_reply in [false, true] {
        let fixture = fixture(false);
        let objects = publish(&fixture, 3).await;
        let (actor, verb, params) = attachment_request(&fixture, is_reply, &objects[..2]).await;
        let first = dispatch(&fixture, actor, verb, params.clone()).await;
        assert_copy_rows(&fixture, &first, &objects[..2]).await;
        let intact = dispatch(&fixture, actor, verb, params.clone()).await;
        assert_replayed(&first, &intact);
        let before = attachment_replay_publication_snapshot(&fixture).await;
        for changed_refs in [
            vec![objects[0].0.as_str(), objects[2].0.as_str()],
            vec![objects[1].0.as_str(), objects[0].0.as_str()],
        ] {
            let mut changed_request = params.clone();
            changed_request["attachments"] = json!(changed_refs);
            let result = fixture
                .registry
                .dispatch_with_identity(verb, changed_request, Some(identity(actor)))
                .await;
            assert!(
                result.is_err(),
                "a different valid ref list or order must conflict: {result:?}"
            );
            let error = result.unwrap_err();
            let RuntimeError::Khive(conflict) = error.refusal_source() else {
                panic!("expected an intact-pair key conflict, got {error:?}");
            };
            assert_eq!(conflict.kind(), khive_types::ErrorKind::Conflict);
            let details = conflict.details().expect("key conflict details");
            assert_eq!(details.get("reason"), Some("key_conflict"));
            assert_eq!(details.get("existing_id"), first["full_id"].as_str());
            assert_eq!(
                attachment_replay_publication_snapshot(&fixture).await,
                before,
                "conflict must not repair, reorder or replace either copy or publish any rows"
            );
            assert_copy_rows(&fixture, &first, &objects[..2]).await;
        }
    }
}
