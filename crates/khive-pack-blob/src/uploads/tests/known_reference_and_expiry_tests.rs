#[tokio::test]
async fn existing_expected_reference_returns_stored_size_without_staging() {
    let f = fixture();
    let reference = f.store.put(b"existing".to_vec()).await.unwrap();
    let result = f
        .manager
        .begin(1, Some(reference.clone()), "uploader:a".into())
        .await
        .unwrap();
    assert_eq!(
        result,
        json!({"content_ref": reference.to_string(), "size": 8})
    );
    assert!(result.get("upload_id").is_none());
    assert!(f.manager.records.lock().unwrap().is_empty());
    assert!(!f.root.join(".uploads").exists());
}

#[tokio::test]
async fn existing_expected_reference_restarts_publish_grace() {
    let f = fixture();
    let reference = f.store.put(b"existing".to_vec()).await.unwrap();
    let path = f
        .root
        .join(&reference.as_str()[..2])
        .join(&reference.as_str()[2..4])
        .join(reference.as_str());
    age_file(&path);
    let old = std::fs::metadata(&path).unwrap().modified().unwrap();

    let result = f
        .manager
        .begin(1, Some(reference.clone()), "uploader:a".into())
        .await
        .unwrap();
    assert_eq!(
        result,
        json!({"content_ref": reference.to_string(), "size": 8})
    );
    assert!(std::fs::metadata(&path).unwrap().modified().unwrap() > old);
    assert!(!f.root.join(".uploads").exists());
}

#[tokio::test]
async fn abort_removes_staging_and_consumes_capability() {
    let f = fixture();
    let id = begin(&f.manager, 3).await;
    f.manager.put_part(&id, 0, b"abc".to_vec()).await.unwrap();
    assert_eq!(
        f.manager.abort(&id).await.unwrap(),
        json!({"aborted": true})
    );
    assert!(!staged(&f.root, &id).exists());
    assert_unknown(f.manager.abort(&id).await.unwrap_err());
    assert_unknown(f.manager.commit(&id).await.unwrap_err());
}

#[tokio::test]
async fn verb_expiry_rejects_stale_part_without_a_sweeper() {
    let f = fixture();
    let id = begin(&f.manager, 2).await;
    f.manager.put_part(&id, 0, b"a".to_vec()).await.unwrap();
    expire_record(&f.manager, &id).await;
    assert!(staged(&f.root, &id).exists());
    assert_unknown(f.manager.put_part(&id, 1, b"b".to_vec()).await.unwrap_err());
    assert!(!staged(&f.root, &id).exists());
    assert_unknown(f.manager.commit(&id).await.unwrap_err());
}

#[tokio::test]
async fn verb_expiry_rejects_stale_complete_commit_without_a_sweeper() {
    let f = fixture();
    let id = begin(&f.manager, 1).await;
    f.manager.put_part(&id, 0, b"a".to_vec()).await.unwrap();
    expire_record(&f.manager, &id).await;
    assert_unknown(f.manager.commit(&id).await.unwrap_err());
    assert!(!staged(&f.root, &id).exists());
    assert!(!f.store.exists(&content_ref(b"a")).await.unwrap());
}

#[tokio::test]
async fn live_expiry_sweep_removes_staging_without_a_verb_call() {
    let f = fixture();
    let id = begin(&f.manager, 2).await;
    f.manager.put_part(&id, 0, b"a".to_vec()).await.unwrap();
    expire_record(&f.manager, &id).await;
    assert_eq!(f.manager.sweep().await.unwrap(), 1);
    assert!(!staged(&f.root, &id).exists());
    assert!(!f.manager.records.lock().unwrap().contains_key(&id));
    assert_eq!(f.manager.sweep().await.unwrap(), 0);
}

#[tokio::test]
async fn restart_orphan_sweep_removes_staging_without_a_live_record() {
    let f = fixture();
    let id = begin(&f.manager, 2).await;
    f.manager.put_part(&id, 0, b"a".to_vec()).await.unwrap();
    let runtime = f.manager.runtime.clone();
    drop(f.manager);
    let mut restarted = UploadManager::new(runtime);
    restarted.policy = UploadPolicy {
        idle_for: IDLE,
        sweep_interval: Duration::from_secs(600),
        max_active: 128,
        max_per_actor: 16,
    };
    assert_unknown(restarted.commit(&id).await.unwrap_err());
    assert!(staged(&f.root, &id).exists());
    age_file(&staged(&f.root, &id));
    // Deliberate legacy recovery leftover, not a live ownerless producer.
    std::fs::remove_file(staged(&f.root, &id).with_extension("lease")).unwrap();
    assert_eq!(restarted.sweep().await.unwrap(), 1);
    assert!(!staged(&f.root, &id).exists());
    let new_id = begin(&restarted, 2).await;
    assert_ne!(new_id, id);
    restarted
        .put_part(&new_id, 0, b"ab".to_vec())
        .await
        .unwrap();
    assert_eq!(
        restarted.commit(&new_id).await.unwrap()["content_ref"],
        content_ref(b"ab").to_string()
    );
}

#[tokio::test]
async fn sweep_preserves_fresh_upload_and_committed_object() {
    let f = fixture();
    let committed = f.store.put(b"committed".to_vec()).await.unwrap();
    let id = begin(&f.manager, 2).await;
    f.manager.put_part(&id, 0, b"a".to_vec()).await.unwrap();
    assert_eq!(f.manager.sweep().await.unwrap(), 0);
    assert_eq!(std::fs::read(staged(&f.root, &id)).unwrap(), b"a");
    assert!(f.store.exists(&committed).await.unwrap());
    f.manager.abort(&id).await.unwrap();
}

#[tokio::test]
async fn unconfigured_runtime_refuses_every_upload_operation() {
    let manager = UploadManager::new(KhiveRuntime::memory().unwrap());
    let id = UploadId::from_bytes(&[0; 16]);
    for result in [
        manager.begin(0, None, "uploader:a".into()).await,
        manager.put_part(&id, 0, Vec::new()).await,
        manager.commit(&id).await,
        manager.abort(&id).await,
        manager.sweep().await.map(Value::from),
    ] {
        let error = result.unwrap_err();
        assert!(matches!(error, RuntimeError::Unconfigured(_)), "{error}");
    }
    assert!(manager.records.lock().unwrap().is_empty());
}
