#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(config_ledger)]
async fn idempotency_concurrent_sends_leave_one_pair_and_no_fts_orphans() {
    let (_dir, sender, _, runtime) = file_parties();
    for round in 0..8 {
        let params = args(&format!("concurrent-{round}"));
        let (left, right) = tokio::join!(
            sender.dispatch("comm.send", params.clone()),
            sender.dispatch("comm.send", params)
        );
        let (left, right) = (left.unwrap(), right.unwrap());
        assert_eq!(left["full_id"], right["full_id"]);
        assert_eq!(left["recipient_id"], right["recipient_id"]);
        assert_ne!(left["replayed"], right["replayed"]);
        assert_eq!(message_count(&runtime, false).await, (round + 1) * 2);
        assert_eq!(message_count(&runtime, true).await, round + 1);
    }
    for table in ["fts_notes", "fts_notes_rowids"] {
        let count = runtime
            .sql()
            .reader()
            .await
            .unwrap()
            .query_scalar(SqlStatement {
                sql: format!("SELECT count(*) FROM {table}"),
                params: vec![],
                label: Some("idempotency-fts-population".into()),
            })
            .await
            .unwrap();
        assert!(
            matches!(count, Some(SqlValue::Integer(16))),
            "orphan rows in {table}: {count:?}"
        );
    }
}

async fn parent(sender: &VerbRegistry, recipient: &VerbRegistry) -> String {
    let sent = sender
        .dispatch(
            "comm.send",
            json!({"to":"actor:recipient","subject":"parent","content":"parent"}),
        )
        .await
        .unwrap();
    let rows = recipient
        .dispatch(
            "comm.inbox",
            json!({"status":"all","thread_id":sent["thread_id"]}),
        )
        .await
        .unwrap();
    rows["messages"][0]["full_id"].as_str().unwrap().to_owned()
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn idempotency_reply_replays_alias_without_marking_parent_again() {
    let (sender, recipient, runtime) = parties();
    let parent_id = parent(&sender, &recipient).await;
    let params = json!({"id":parent_id,"content":"answer","idempotency_key":"reply-key"});
    let original = recipient
        .dispatch("comm.reply", params.clone())
        .await
        .unwrap();
    assert_eq!(original["marked_read"], true);
    let before = snapshot(&runtime).await;
    let mut alias = params.clone();
    alias["id"] = json!(&parent_id[..8]);
    let replay = recipient.dispatch("comm.reply", alias).await.unwrap();
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["full_id"], original["full_id"]);
    assert_eq!(replay["recipient_id"], original["recipient_id"]);
    assert!(replay["marked_read"].is_null());
    assert_eq!(
        snapshot(&runtime).await,
        before,
        "replay repeated parent mark-read"
    );
    let mut changed = params;
    changed["content"] = json!("other answer");
    conflict(
        recipient.dispatch("comm.reply", changed).await.unwrap_err(),
        &original["full_id"],
    );
    assert_eq!(snapshot(&runtime).await, before);
    let other_parent = parent(&sender, &recipient).await;
    let before = snapshot(&runtime).await;
    conflict(
        recipient
            .dispatch(
                "comm.reply",
                json!({"id":other_parent,"content":"answer","idempotency_key":"reply-key"}),
            )
            .await
            .unwrap_err(),
        &original["full_id"],
    );
    conflict(recipient.dispatch("comm.send",json!({"to":"actor:sender","subject":"Re: parent","content":"answer","thread_id":original["thread_id"],"idempotency_key":"reply-key"})).await.unwrap_err(),&original["full_id"]);
    assert_eq!(
        snapshot(&runtime).await,
        before,
        "conflicting reply/send wrote notes"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(config_ledger)]
async fn idempotency_concurrent_replies_preserve_one_response_pair() {
    let (_dir, sender, recipient, runtime) = file_parties();
    let parent_id = parent(&sender, &recipient).await;
    let params = json!({"id":parent_id,"content":"answer","idempotency_key":"reply-race"});
    let (left, right) = tokio::join!(
        recipient.dispatch("comm.reply", params.clone()),
        recipient.dispatch("comm.reply", params)
    );
    let (left, right) = (left.unwrap(), right.unwrap());
    assert_eq!(left["full_id"], right["full_id"]);
    assert_eq!(left["recipient_id"], right["recipient_id"]);
    assert_ne!(left["replayed"], right["replayed"]);
    assert_eq!(message_count(&runtime, false).await, 4);
    assert_eq!(message_count(&runtime, true).await, 1);
}
