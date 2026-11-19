fn assert_acknowledgement_only(read: &Value) {
    let object = read.as_object().expect("read acknowledgement object");
    assert_eq!(
        object.len(),
        5,
        "body=false preserves the base acknowledgement fields"
    );
    for field in ["id", "full_id", "status", "read", "properties"] {
        assert!(
            object.contains_key(field),
            "base acknowledgement has {field}"
        );
    }
    assert_eq!(read["status"], "success");
    assert_eq!(read["read"], true);
    assert_eq!(read["properties"]["read"], true);
    assert!(read.get("attachments").is_none());
    assert!(read.get("content").is_none());
}

#[tokio::test]
async fn attachment_read_body_opt_out_preserves_acknowledgement_shape() {
    let fixture = fixture(false);
    let objects = publish(&fixture, 2).await;
    let (actor, verb, params) = attachment_request(&fixture, false, &objects).await;
    let sent = dispatch(&fixture, actor, verb, params).await;
    assert_copy_rows(&fixture, &sent, &objects).await;
    let ack = dispatch(
        &fixture,
        RECIPIENT,
        "comm.read",
        json!({"id": sent["recipient_id"], "body": false}),
    )
    .await;
    assert_acknowledgement_only(&ack);
    assert_eq!(ack["full_id"], sent["recipient_id"]);
    let read = dispatch(
        &fixture,
        RECIPIENT,
        "comm.read",
        json!({"id": sent["recipient_id"], "body": true}),
    )
    .await;
    assert_eq!(read["status"], "success");
    assert_eq!(read["full_id"], ack["full_id"]);
    assert_eq!(read["content"], "attached send");
    assert_metadata(&read, &objects);
    let default_read = dispatch(
        &fixture,
        RECIPIENT,
        "comm.read",
        json!({"id": sent["recipient_id"]}),
    )
    .await;
    assert_metadata(&default_read, &objects);
}

#[tokio::test]
async fn attachment_free_messages_have_empty_metadata_arrays() {
    for explicit_empty in [false, true] {
        let fixture = fixture(false);
        let mut params = json!({
            "to": RECIPIENT,
            "content": "message without files",
            "idempotency_key": "empty-attachments",
        });
        if explicit_empty {
            params["attachments"] = json!([]);
        }
        let sent = dispatch(&fixture, SENDER, "comm.send", params).await;
        assert_views(&fixture, &sent, &[]).await;
        assert_eq!(
            population(&fixture.runtime).await,
            Population {
                notes: 2,
                attachments: 0
            }
        );
    }
}

#[tokio::test]
async fn attachment_free_read_acknowledgement_keeps_base_shape() {
    let fixture = fixture(false);
    let sent = reply_root(&fixture).await;
    let ack = dispatch(
        &fixture,
        RECIPIENT,
        "comm.read",
        json!({"id": sent["recipient_id"], "body": false}),
    )
    .await;
    assert_acknowledgement_only(&ack);
    assert_eq!(ack["full_id"], sent["recipient_id"]);
}
