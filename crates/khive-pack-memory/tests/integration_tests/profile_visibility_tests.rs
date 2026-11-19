/// A bound serving profile whose record cannot be read (here: no brain pack
/// registered, so `brain.profile` dispatch fails) must NOT be stamped as
/// `served_by_profile_id` — the recall scored with configured defaults, and
/// stamping would credit downstream feedback to a profile that never served.
/// A readable record with a null snapshot (new profile) still stamps; that is
/// the posterior bootstrap path, exercised by the brain pack's own tests.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_unreadable_bound_profile_is_not_stamped_as_served_by() {
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: vec![],
        brain_profile: Some("ghost-profile-v1".to_string()),
        ..RuntimeConfig::default()
    })
    .expect("in-memory runtime");
    let registry = make_registry(rt.clone());

    registry
        .dispatch(
            "memory.remember",
            json!({ "content": "served-by attribution regression fixture" }),
        )
        .await
        .expect("memory.remember succeeds");

    let hits = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "served-by attribution regression fixture" }),
        )
        .await
        .expect("memory.recall succeeds with an unreadable bound profile");
    let hits = hits.as_array().expect("array of hits");
    assert!(!hits.is_empty(), "recall returned at least one result");
    for hit in hits {
        assert!(
            hit.get("served_by_profile_id").is_none() || hit["served_by_profile_id"].is_null(),
            "unreadable bound profile must not be stamped as served_by; got: {hit:?}"
        );
        assert_eq!(
            hit["serve_attribution"],
            json!("unattributed"),
            "failed profile read must be wire-visible and distinct from omission"
        );
    }

    // Explicit unknown profile_id remains a hard per-op error, unchanged.
    let err = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "served-by attribution regression fixture", "profile_id": "ghost-profile-v1" }),
        )
        .await;
    assert!(err.is_err(), "explicit unknown profile_id must error");
}

/// A readable profile record with a null state snapshot is the bootstrap case,
/// not a failed read. It must remain positively attributed on the wire.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_readable_null_snapshot_profile_still_stamps_as_serving() {
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: vec![],
        brain_profile: Some("bootstrap-profile-v1".to_string()),
        ..RuntimeConfig::default()
    })
    .expect("in-memory runtime");

    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(MemoryPack::new(
        receipt_credentials::with_receipt_credentials(rt.clone()),
    ));
    builder.register(NullSnapshotBrainPack);
    let registry = builder.build().expect("registry builds");

    registry
        .dispatch(
            "memory.remember",
            json!({"content": "null snapshot bootstrap attribution fixture"}),
        )
        .await
        .expect("remember fixture");
    let result = registry
        .dispatch(
            "memory.recall",
            json!({"query": "null snapshot bootstrap attribution fixture"}),
        )
        .await
        .expect("readable null-snapshot profile must not fail recall");
    let hits = result.as_array().expect("array of hits");
    assert!(!hits.is_empty());
    assert_eq!(
        hits[0]["served_by_profile_id"],
        json!("bootstrap-profile-v1")
    );
    assert_eq!(hits[0]["serve_attribution"], json!("profile"));
}
