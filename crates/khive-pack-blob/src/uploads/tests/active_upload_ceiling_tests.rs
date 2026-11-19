#[tokio::test]
async fn total_active_upload_ceiling_refuses_before_staging_and_abort_restores_slot() {
    let mut f = fixture();
    f.manager.policy.max_active = 2;
    f.manager.policy.max_per_actor = 3;
    let first = begin(&f.manager, 0).await;
    let second = begin(&f.manager, 0).await;
    assert_upload_ceiling(
        f.manager
            .begin(0, None, "uploader:b".into())
            .await
            .unwrap_err(),
        "total",
        2,
    );
    assert_eq!(staged_count(&f.root), 2);
    let retained_record = f.manager.record(&first).unwrap();
    f.manager.abort(&first).await.unwrap();
    assert!(!staged(&f.root, &first).exists());
    let replacement = upload_id(&f.manager.begin(0, None, "uploader:b".into()).await.unwrap());
    assert!(retained_record.lock().await.slot.is_none());
    assert_ne!(replacement, first);
    assert!(staged(&f.root, &second).exists());
    f.manager.abort(&second).await.unwrap();
    f.manager.abort(&replacement).await.unwrap();
}

#[tokio::test]
async fn per_actor_upload_ceiling_isolated_and_abort_releases_originating_actor_slot() {
    let mut f = fixture();
    f.manager.policy.max_active = 4;
    f.manager.policy.max_per_actor = 1;
    let first = begin(&f.manager, 0).await;
    assert_upload_ceiling(
        f.manager
            .begin(0, None, "uploader:a".into())
            .await
            .unwrap_err(),
        "per-actor",
        1,
    );
    let other = upload_id(&f.manager.begin(0, None, "uploader:b".into()).await.unwrap());
    crate::handlers::handle_abort(&f.manager, json!({"upload_id": first.to_string()}))
        .await
        .unwrap();
    let replacement = begin(&f.manager, 0).await;
    assert_upload_ceiling(
        f.manager
            .begin(0, None, "uploader:b".into())
            .await
            .unwrap_err(),
        "per-actor",
        1,
    );
    assert!(staged(&f.root, &other).exists());
    f.manager.abort(&other).await.unwrap();
    f.manager.abort(&replacement).await.unwrap();
}

#[tokio::test]
async fn committed_upload_releases_total_and_actor_slots() {
    let mut f = fixture();
    f.manager.policy.max_active = 1;
    f.manager.policy.max_per_actor = 1;
    let id = begin(&f.manager, 1).await;
    f.manager.put_part(&id, 0, b"a".to_vec()).await.unwrap();
    let retained_record = f.manager.record(&id).unwrap();
    f.manager.commit(&id).await.unwrap();
    assert!(!staged(&f.root, &id).exists());
    assert!(f.store.exists(&content_ref(b"a")).await.unwrap());
    let replacement = begin(&f.manager, 0).await;
    assert!(retained_record.lock().await.slot.is_none());
    f.manager.abort(&replacement).await.unwrap();
}

#[tokio::test]
async fn known_reference_bypasses_full_upload_ceilings_without_reserving_slot() {
    let mut f = fixture();
    f.manager.policy.max_active = 1;
    f.manager.policy.max_per_actor = 1;
    let id = begin(&f.manager, 0).await;
    let reference = f.store.put(b"known".to_vec()).await.unwrap();
    for actor in ["uploader:a", "uploader:b"] {
        let result = f
            .manager
            .begin(0, Some(reference.clone()), actor.into())
            .await
            .unwrap();
        assert_eq!(
            result,
            json!({"content_ref": reference.to_string(), "size": 5})
        );
        assert!(result.get("upload_id").is_none());
    }
    assert_eq!(f.manager.records.lock().unwrap().len(), 1);
    assert_eq!(staged_count(&f.root), 1);
    assert_upload_ceiling(
        f.manager
            .begin(0, None, "uploader:b".into())
            .await
            .unwrap_err(),
        "total",
        1,
    );
    f.manager.abort(&id).await.unwrap();
    let replacement = begin(&f.manager, 0).await;
    f.manager.abort(&replacement).await.unwrap();
}
