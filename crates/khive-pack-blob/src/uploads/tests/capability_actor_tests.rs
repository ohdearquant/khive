#[tokio::test]
async fn independent_capabilities_allow_cross_actor_append_commit_and_abort() {
    let f = fixture();
    let mut builder = VerbRegistryBuilder::new();
    let pack = BlobPack::new(f.manager.runtime.clone());
    let manager = pack.uploads.clone();
    builder.register(pack);
    let registry = builder.build().unwrap();
    let actor_a = RequestIdentity {
        namespace: "local".into(),
        actor_id: Some("uploader:a".into()),
        ..Default::default()
    };
    let actor_b = RequestIdentity {
        namespace: "other".into(),
        actor_id: Some("uploader:b".into()),
        ..Default::default()
    };
    let first = registry
        .dispatch_with_identity("blob.begin", json!({"size": 3}), Some(actor_a.clone()))
        .await
        .unwrap();
    let second = registry
        .dispatch_with_identity("blob.begin", json!({"size": 3}), Some(actor_b.clone()))
        .await
        .unwrap();
    let first_id = upload_id(&first);
    let second_id = upload_id(&second);
    assert_ne!(first_id, second_id);
    let owner = f
        .manager
        .runtime
        .core()
        .backend()
        .database_owner_identity()
        .unwrap()
        .durable_id();
    for id in [&first_id, &second_id] {
        assert_eq!(lease(&f.root, id)["owner"], owner.to_string());
        assert_eq!(
            lease(&f.root, id)["idle_secs"],
            manager.policy.idle_for.as_secs()
        );
    }
    registry
        .dispatch_with_identity(
            "blob.put_part",
            json!({"upload_id": first_id.to_string(), "index": 0, "bytes": BASE64.encode(b"abc")}),
            Some(actor_b.clone()),
        )
        .await
        .unwrap();
    let clock = manager.record(&first_id).unwrap().lock().await.last_part;
    let retry = registry
        .dispatch_with_identity(
            "blob.put_part",
            json!({"upload_id": first_id.to_string(), "index": 0, "bytes": BASE64.encode(b"abc")}),
            Some(actor_a.clone()),
        )
        .await
        .unwrap();
    assert_eq!(retry, json!({"next_index": 1, "received_bytes": 3}));
    assert_eq!(lease(&f.root, &first_id)["renew_seq"], 2);
    assert!(manager.record(&first_id).unwrap().lock().await.last_part > clock);
    registry
        .dispatch_with_identity(
            "blob.put_part",
            json!({"upload_id": second_id.to_string(), "index": 0, "bytes": BASE64.encode(b"def")}),
            Some(actor_a.clone()),
        )
        .await
        .unwrap();
    registry
        .dispatch_with_identity(
            "blob.abort",
            json!({"upload_id": second_id.to_string()}),
            Some(actor_a),
        )
        .await
        .unwrap();
    assert!(!staged(&f.root, &second_id).exists());
    assert_eq!(std::fs::read(staged(&f.root, &first_id)).unwrap(), b"abc");
    let result = registry
        .dispatch_with_identity(
            "blob.commit",
            json!({"upload_id": first_id.to_string()}),
            Some(actor_b),
        )
        .await
        .unwrap();
    assert_eq!(result["content_ref"], content_ref(b"abc").to_string());
    assert!(!f.store.exists(&content_ref(b"def")).await.unwrap());
}
