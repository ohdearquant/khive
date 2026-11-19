#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn idempotency_actor_namespace_and_delimiter_tuples_are_distinct() {
    let backend = khive_db::StorageBackend::memory().unwrap();
    backend.pool().run_migrations().unwrap();
    let backend = Arc::new(backend);
    let (a, runtime) = actor_registry(Arc::clone(&backend), "actor:a", "local");
    let (b, _) = actor_registry(Arc::clone(&backend), "actor:a:b", "local");
    let (c, _) = actor_registry(backend, "actor:a", "other-ns");
    let mut ids = std::collections::BTreeSet::new();
    for (registry, key, namespace) in [
        (&a, "b:c", "local"),
        (&b, "c", "local"),
        (&b, "b:c", "local"),
        (&c, "b:c", "other-ns"),
    ] {
        let mut params = args(key);
        params["namespace"] = json!(namespace);
        let first = registry
            .dispatch("comm.send", params.clone())
            .await
            .unwrap();
        assert!(ids.insert(first["full_id"].as_str().unwrap().to_owned()));
        let again = registry.dispatch("comm.send", params).await.unwrap();
        assert_eq!(again["full_id"], first["full_id"]);
        assert_eq!(again["replayed"], true);
    }
    assert_eq!(message_count(&runtime, false).await, 8);
    assert_eq!(message_count(&runtime, true).await, 4);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn idempotency_key_validation_self_send_and_unkeyed_controls() {
    let (sender, _, runtime) = parties();
    for key in ["k".repeat(513), "λ".repeat(257), "nul\0key".into()] {
        assert!(sender.dispatch("comm.send", args(&key)).await.is_err());
        assert_eq!(message_count(&runtime, false).await, 0);
    }
    for key in [
        String::new(),
        "k".repeat(512),
        "λ".repeat(256),
        "line\n\t\rkey".into(),
    ] {
        let first = sender.dispatch("comm.send", args(&key)).await.unwrap();
        let again = sender.dispatch("comm.send", args(&key)).await.unwrap();
        assert_eq!(first["full_id"], again["full_id"]);
        assert_eq!(again["idempotency_key"], key);
    }
    let self_args =
        json!({"to":"actor:sender","content":"self","self_send":true,"idempotency_key":"self-key"});
    let first = sender
        .dispatch("comm.send", self_args.clone())
        .await
        .unwrap();
    let again = sender.dispatch("comm.send", self_args).await.unwrap();
    assert_eq!(first["full_id"], again["full_id"]);
    let before = message_count(&runtime, false).await;
    let unkeyed = json!({"to":"actor:recipient","content":"intentional repeat"});
    let first = sender.dispatch("comm.send", unkeyed.clone()).await.unwrap();
    let second = sender.dispatch("comm.send", unkeyed).await.unwrap();
    assert_ne!(first["full_id"], second["full_id"]);
    assert!(first.get("replayed").is_none());
    assert_eq!(message_count(&runtime, false).await, before + 4);
}
