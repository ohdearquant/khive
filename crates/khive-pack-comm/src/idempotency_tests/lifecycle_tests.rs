#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn idempotency_missing_or_altered_sibling_refuses_without_repair() {
    for damage in [
        "missing",
        "content",
        "thread",
        "reference",
        "actor",
        "physical-key",
    ] {
        let (sender, _, runtime) = parties();
        let first = sender.dispatch("comm.send", args("intact")).await.unwrap();
        let token = runtime.authorize(Namespace::local()).unwrap();
        let store = runtime.notes(&token).unwrap();
        let recipient = first["recipient_id"]
            .as_str()
            .unwrap()
            .parse::<Uuid>()
            .unwrap();
        if damage == "missing" {
            assert!(store
                .delete_note(recipient, DeleteMode::Hard)
                .await
                .unwrap());
        } else {
            let mut note = store.get_note(recipient).await.unwrap().unwrap();
            match damage {
                "content" => note.content = "tampered".into(),
                "thread" => note.properties.as_mut().unwrap()["thread_id"] = json!(Uuid::new_v4()),
                "reference" => {
                    note.properties.as_mut().unwrap()["outbound_ref"] = json!(Uuid::new_v4())
                }
                "actor" => note.properties.as_mut().unwrap()["to_actor"] = json!("actor:else"),
                "physical-key" => {
                    // Ordinary UPSERT deliberately preserves an existing key.
                    // Corrupt the isolated fixture through SQL and verify it below.
                    runtime
                        .sql()
                        .writer()
                        .await
                        .unwrap()
                        .execute(SqlStatement {
                            sql:
                                "UPDATE notes SET key = 'unexpected-recipient-claim' WHERE id = ?1"
                                    .into(),
                            params: vec![SqlValue::Text(recipient.to_string())],
                            label: Some("corrupt-test-sibling-key".into()),
                        })
                        .await
                        .unwrap();
                }
                _ => unreachable!(),
            }
            store.upsert_note(note).await.unwrap();
        }
        if damage == "physical-key" {
            assert_eq!(
                store
                    .get_note(recipient)
                    .await
                    .unwrap()
                    .unwrap()
                    .key
                    .as_deref(),
                Some("unexpected-recipient-claim")
            );
        }
        let before = snapshot(&runtime).await;
        conflict(
            sender
                .dispatch("comm.send", args("intact"))
                .await
                .expect_err(&format!("replay accepted {damage} sibling")),
            &first["full_id"],
        );
        assert_eq!(
            snapshot(&runtime).await,
            before,
            "repaired damaged sibling: {damage}"
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn idempotency_outbound_deletion_releases_live_claim() {
    for mode in [DeleteMode::Soft, DeleteMode::Hard] {
        let (sender, _, runtime) = parties();
        let first = sender
            .dispatch("comm.send", args("released"))
            .await
            .unwrap();
        let token = runtime.authorize(Namespace::local()).unwrap();
        assert!(runtime
            .notes(&token)
            .unwrap()
            .delete_note(first["full_id"].as_str().unwrap().parse().unwrap(), mode)
            .await
            .unwrap());
        let next = sender
            .dispatch("comm.send", args("released"))
            .await
            .unwrap();
        assert_ne!(first["full_id"], next["full_id"]);
        assert_eq!(next["replayed"], false);
        assert_eq!(
            message_count(&runtime, false).await,
            3,
            "old inbound remains beside new pair"
        );
        assert_eq!(message_count(&runtime, true).await, 1);
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn idempotency_readback_and_delivery_changes_preserve_replay() {
    let (sender, recipient, runtime) = parties();
    let first = sender
        .dispatch("comm.send", args("read-back"))
        .await
        .unwrap();
    for (registry, mailbox) in [(&sender, "sent"), (&recipient, "inbox")] {
        let result = registry
            .dispatch(
                "comm.inbox",
                json!({"box":mailbox,"fields":["full_id","idempotency_key"]}),
            )
            .await
            .unwrap();
        assert_eq!(result["messages"][0]["idempotency_key"], "read-back");
        let thread = registry
            .dispatch("comm.thread", json!({"id":first["thread_id"]}))
            .await
            .unwrap();
        let messages = thread["messages"].as_array().unwrap();
        assert!(!messages.is_empty());
        assert!(messages
            .iter()
            .all(|note| note["properties"]["idempotency_key"] == "read-back"));
    }
    let read = recipient
        .dispatch("comm.read", json!({"id":first["recipient_id"]}))
        .await
        .unwrap();
    assert_eq!(read["properties"]["idempotency_key"], "read-back");
    assert!(sender
        .dispatch("comm.read", json!({"id":first["full_id"]}))
        .await
        .is_err());
    let token = runtime.authorize(Namespace::local()).unwrap();
    runtime
        .notes(&token)
        .unwrap()
        .set_note_property(
            first["full_id"].as_str().unwrap().parse().unwrap(),
            "delivered_at",
            json!("2026-09-09T00:00:00Z"),
            1,
        )
        .await
        .unwrap();
    let before = snapshot(&runtime).await;
    let replay = sender
        .dispatch("comm.send", args("read-back"))
        .await
        .unwrap();
    assert_eq!(replay["replayed"], true);
    assert_eq!(snapshot(&runtime).await, before);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn idempotency_replay_does_not_publish_an_inbox_signal() {
    let (_, _, runtime) = parties();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let signal = crate::inbox_signal::InboxSignal::new();
    crate::handlers::handle_send(&runtime, &signal, &token, args("signal"))
        .await
        .unwrap();
    assert_eq!(signal.snapshot(), 1);
    let before = snapshot(&runtime).await;
    let replay = crate::handlers::handle_send(&runtime, &signal, &token, args("signal"))
        .await
        .unwrap();
    assert_eq!(replay["replayed"], true);
    assert_eq!(signal.snapshot(), 1);
    assert_eq!(snapshot(&runtime).await, before);
}
