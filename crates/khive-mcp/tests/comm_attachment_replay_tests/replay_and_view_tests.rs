#[tokio::test]
async fn keyed_send_attachment_replay_preserves_both_copies() {
    let fixture = fixture(false);
    let objects = publish(&fixture, 2).await;
    let (actor, verb, params) = attachment_request(&fixture, false, &objects).await;
    let first = dispatch(&fixture, actor, verb, params.clone()).await;
    assert_copy_rows(&fixture, &first, &objects).await;
    let before = population(&fixture.runtime).await;
    assert_eq!(
        before,
        Population {
            notes: 2,
            attachments: 4
        }
    );
    let replay = dispatch(&fixture, actor, verb, params).await;
    assert_replayed(&first, &replay);
    assert_eq!(
        population(&fixture.runtime).await,
        before,
        "replay adds no rows"
    );
    assert_copy_rows(&fixture, &replay, &objects).await;
}

#[tokio::test]
async fn keyed_reply_attachment_replay_preserves_both_copies() {
    let fixture = fixture(false);
    let objects = publish(&fixture, 2).await;
    let (actor, verb, params) = attachment_request(&fixture, true, &objects).await;
    let first = dispatch(&fixture, actor, verb, params.clone()).await;
    assert_copy_rows(&fixture, &first, &objects).await;
    let before = population(&fixture.runtime).await;
    assert_eq!(
        before,
        Population {
            notes: 4,
            attachments: 4
        }
    );
    let replay = dispatch(&fixture, actor, verb, params).await;
    assert_replayed(&first, &replay);
    assert_eq!(
        population(&fixture.runtime).await,
        before,
        "replay adds no rows"
    );
    assert_copy_rows(&fixture, &replay, &objects).await;
}

#[tokio::test]
async fn keyed_attachment_replay_refuses_missing_recipient_row() {
    for is_reply in [false, true] {
        let fixture = fixture(false);
        let objects = publish(&fixture, 2).await;
        let (actor, verb, params) = attachment_request(&fixture, is_reply, &objects).await;
        let first = dispatch(&fixture, actor, verb, params.clone()).await;
        assert_copy_rows(&fixture, &first, &objects).await;
        let outbound = full_id(&first, "full_id");
        let inbound = full_id(&first, "recipient_id");
        let store = fixture.runtime.attachments().expect("main attachments");
        assert!(store
            .delete_attachment(inbound, "message-attachment:1")
            .await
            .expect("remove recipient attachment"));
        let before = population(&fixture.runtime).await;
        let outbound_rows = store.list_attachments(outbound).await.unwrap();
        let inbound_rows = store.list_attachments(inbound).await.unwrap();
        assert_eq!(outbound_rows.len(), 2);
        assert_eq!(inbound_rows.len(), 1);
        let error = fixture
            .registry
            .dispatch_with_identity(verb, params, Some(identity(actor)))
            .await
            .expect_err("an incomplete attachment pair must conflict");
        let RuntimeError::Khive(conflict) = error.refusal_source() else {
            panic!("expected a structured key conflict, got {error:?}");
        };
        assert_eq!(conflict.kind(), khive_types::ErrorKind::Conflict);
        let details = conflict.details().expect("key conflict details");
        assert_eq!(details.get("reason"), Some("key_conflict"));
        assert_eq!(details.get("existing_id"), first["full_id"].as_str());
        assert_eq!(
            population(&fixture.runtime).await,
            before,
            "conflict adds no rows"
        );
        assert_eq!(
            store.list_attachments(outbound).await.unwrap(),
            outbound_rows
        );
        assert_eq!(store.list_attachments(inbound).await.unwrap(), inbound_rows);
        assert!(
            store
                .get_attachment(inbound, "message-attachment:1")
                .await
                .unwrap()
                .is_none(),
            "replay does not repair the missing row"
        );
    }
}

async fn assert_main_attachment_operation(is_reply: bool) {
    let fixture = fixture(false);
    let objects = publish(&fixture, 2).await;
    let (actor, verb, params) = attachment_request(&fixture, is_reply, &objects).await;
    let first = dispatch(&fixture, actor, verb, params).await;
    assert_copy_rows(&fixture, &first, &objects).await;
}

async fn assert_split_attachment_refusal(is_reply: bool) {
    assert_main_attachment_operation(is_reply).await;
    let fixture = fixture(true);
    let objects = publish(&fixture, 2).await;
    let (actor, verb, params) = attachment_request(&fixture, is_reply, &objects).await;
    let core = fixture.runtime.core();
    let before_comm = population(&fixture.runtime).await;
    let before_main = population(&core).await;
    assert_eq!(before_comm.attachments, 0);
    assert_eq!(before_comm.notes, if is_reply { 2 } else { 0 });
    assert_eq!(
        before_main,
        Population {
            notes: 0,
            attachments: 0
        }
    );
    let error = fixture
        .registry
        .dispatch_with_identity(verb, params, Some(identity(actor)))
        .await
        .expect_err("secondary comm attachments must refuse before writes");
    let RuntimeError::InvalidInput(reason) = error.refusal_source() else {
        panic!("expected invalid attachment parameters, got {error:?}");
    };
    assert!(
        reason.contains(verb),
        "refusal names the operation: {reason}"
    );
    assert!(
        reason.contains("canonical main comm backend"),
        "specific placement refusal: {reason}"
    );
    assert!(
        !reason.contains("unknown field"),
        "unsupported parameters are not placement proof"
    );
    assert_eq!(population(&fixture.runtime).await, before_comm);
    assert_eq!(population(&core).await, before_main);
}

#[tokio::test]
async fn split_comm_backend_refuses_attachment_send_before_writes() {
    assert_split_attachment_refusal(false).await;
}

#[tokio::test]
async fn split_comm_backend_refuses_attachment_reply_before_writes() {
    assert_split_attachment_refusal(true).await;
}

fn mailbox_params(box_name: &str) -> Value {
    let mut params = json!({"box": box_name});
    if box_name == "inbox" {
        params["status"] = json!("all");
    }
    params
}

async fn inbox(fixture: &Fixture, actor: &str, box_name: &str) -> Value {
    dispatch(fixture, actor, "comm.inbox", mailbox_params(box_name)).await
}

async fn assert_views(fixture: &Fixture, sent: &Value, objects: &[(ContentRef, u64)]) {
    assert_ne!(sent["full_id"], sent["recipient_id"]);
    for (actor, box_name, expected_id, direction) in [
        (SENDER, "sent", &sent["full_id"], "outbound"),
        (RECIPIENT, "inbox", &sent["recipient_id"], "inbound"),
    ] {
        let view = inbox(fixture, actor, box_name).await;
        let messages = view["messages"].as_array().expect("mailbox messages");
        assert_eq!(messages.len(), 1);
        assert_eq!(&messages[0]["full_id"], expected_id);
        assert_eq!(messages[0]["from"], SENDER);
        assert_eq!(messages[0]["to"], RECIPIENT);
        assert_eq!(messages[0]["direction"], direction);
        assert_eq!(messages[0]["properties"]["from_actor"], SENDER);
        assert_eq!(messages[0]["properties"]["to_actor"], RECIPIENT);
        if box_name == "inbox" {
            assert_eq!(messages[0]["properties"]["outbound_ref"], sent["full_id"]);
        }
        assert_metadata(&messages[0], objects);
        let mut projected_params = mailbox_params(box_name);
        projected_params["fields"] = json!(["full_id", "attachments"]);
        let projected = dispatch(fixture, actor, "comm.inbox", projected_params).await;
        assert_eq!(projected["messages"].as_array().unwrap().len(), 1);
        assert_eq!(projected["messages"][0]["full_id"], *expected_id);
        assert_metadata(&projected["messages"][0], objects);
        // Both copies are visible here; thread deduplication keeps the outbound twin.
        let full_thread = dispatch(fixture, actor, "comm.thread", json!({"id": expected_id})).await;
        assert_eq!(full_thread["thread_id"], sent["thread_id"]);
        assert_eq!(full_thread["count"], 1);
        let thread_messages = full_thread["messages"].as_array().unwrap();
        assert_eq!(thread_messages.len(), 1);
        assert_eq!(thread_messages[0]["full_id"], sent["full_id"]);
        assert_ne!(thread_messages[0]["full_id"], sent["recipient_id"]);
        assert_eq!(thread_messages[0]["from"], SENDER);
        assert_eq!(thread_messages[0]["to"], RECIPIENT);
        assert_eq!(thread_messages[0]["direction"], "outbound");
        assert_eq!(thread_messages[0]["properties"]["from_actor"], SENDER);
        assert_eq!(thread_messages[0]["properties"]["to_actor"], RECIPIENT);
        assert_metadata(&thread_messages[0], objects);
        let thread = dispatch(
            fixture,
            actor,
            "comm.thread",
            json!({"id": sent["thread_id"], "fields": ["full_id", "attachments"]}),
        )
        .await;
        assert_eq!(thread["thread_id"], sent["thread_id"]);
        assert_eq!(thread["count"], 1);
        assert_eq!(thread["messages"].as_array().unwrap().len(), 1);
        assert_eq!(thread["messages"][0]["full_id"], sent["full_id"]);
        assert_metadata(&thread["messages"][0], objects);
    }
}

#[tokio::test]
async fn eight_attachment_refs_keep_caller_order_and_ignore_other_roles() {
    let fixture = fixture(false);
    let mut objects = publish(&fixture, 8).await;
    objects.sort_by_key(|(reference, _)| std::cmp::Reverse(reference.clone()));
    let (actor, verb, params) = attachment_request(&fixture, false, &objects).await;
    let sent = dispatch(&fixture, actor, verb, params).await;
    assert_copy_rows(&fixture, &sent, &objects).await;
    assert_eq!(
        population(&fixture.runtime).await,
        Population {
            notes: 2,
            attachments: 16
        }
    );
    let stray = fixture
        .blobs
        .put(b"other role object".to_vec())
        .await
        .unwrap();
    assert_eq!(fixture.blobs.size(&stray).await.unwrap(), Some(17));
    let store = fixture.runtime.attachments().expect("main attachments");
    let stray_roles = [
        "message-attachment:8",
        "message-attachment:x",
        "message-attachment:00",
        "message-attachment:7x",
        "xmessage-attachment:0",
        "quarantine-original",
    ];
    for field in ["full_id", "recipient_id"] {
        let id = full_id(&sent, field);
        for role in stray_roles {
            store
                .upsert_attachment(Attachment::from_new(
                    id,
                    AttachmentSubstrate::Note,
                    NewAttachment {
                        role: role.into(),
                        content_ref: stray.clone(),
                        media_type: Some("application/octet-stream".into()),
                        size_bytes: Some(17),
                    },
                    1,
                ))
                .await
                .expect("seed a distinct existing attachment role");
        }
        assert_eq!(store.list_attachments(id).await.unwrap().len(), 14);
    }
    assert_views(&fixture, &sent, &objects).await;
    let read = dispatch(
        &fixture,
        RECIPIENT,
        "comm.read",
        json!({"id": sent["recipient_id"], "body": true}),
    )
    .await;
    assert_eq!(read["status"], "success");
    assert_metadata(&read, &objects);
}
