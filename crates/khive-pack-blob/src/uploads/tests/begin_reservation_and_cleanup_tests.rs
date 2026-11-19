#[tokio::test]
async fn concurrent_begins_reserve_total_and_actor_slots_before_backend_returns() {
    let f = fixture();
    let store = Arc::new(ControlledStore::new(f.store.clone()));
    store.pause_after_begin.store(true, Ordering::SeqCst);
    let mut manager = manager_with_store(durable_runtime(f._dir.path()), store.clone());
    manager.policy.max_active = 2;
    manager.policy.max_per_actor = 1;
    let manager = Arc::new(manager);
    let first = tokio::spawn({
        let manager = manager.clone();
        async move { manager.begin(0, None, "uploader:a".into()).await }
    });
    tokio::time::timeout(Duration::from_secs(10), store.begun.notified())
        .await
        .unwrap();
    assert!(manager.records.lock().unwrap().is_empty());
    assert_upload_ceiling(
        tokio::time::timeout(
            Duration::from_secs(10),
            manager.begin(0, None, "uploader:a".into()),
        )
        .await
        .unwrap()
        .unwrap_err(),
        "per-actor",
        1,
    );
    let second = tokio::spawn({
        let manager = manager.clone();
        async move { manager.begin(0, None, "uploader:b".into()).await }
    });
    tokio::time::timeout(Duration::from_secs(10), store.begun.notified())
        .await
        .unwrap();
    assert!(manager.records.lock().unwrap().is_empty());
    assert_eq!(store.begin_calls.load(Ordering::SeqCst), 2);
    assert_upload_ceiling(
        tokio::time::timeout(
            Duration::from_secs(10),
            manager.begin(0, None, "uploader:c".into()),
        )
        .await
        .unwrap()
        .unwrap_err(),
        "total",
        2,
    );
    assert_eq!(store.begin_calls.load(Ordering::SeqCst), 2);
    assert_eq!(staged_count(&f.root), 2);
    store.pause_after_begin.store(false, Ordering::SeqCst);
    store.release_begin.notify_waiters();
    let first_id = upload_id(
        &tokio::time::timeout(Duration::from_secs(10), first)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
    );
    let second_id = upload_id(
        &tokio::time::timeout(Duration::from_secs(10), second)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
    );
    assert_ne!(first_id, second_id);
    assert_eq!(manager.records.lock().unwrap().len(), 2);
    manager.abort(&first_id).await.unwrap();
    manager.abort(&second_id).await.unwrap();
}

#[tokio::test]
async fn failed_backend_begin_releases_total_and_actor_reservation() {
    let f = fixture();
    let store = Arc::new(ControlledStore::new(f.store.clone()));
    store.begin_failures.store(1, Ordering::SeqCst);
    let mut manager = manager_with_store(durable_runtime(f._dir.path()), store.clone());
    manager.policy.max_active = 1;
    manager.policy.max_per_actor = 1;
    let error = manager
        .begin(0, None, "uploader:a".into())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("injected begin failure"));
    assert!(manager.records.lock().unwrap().is_empty());
    assert!(!f.root.join(".uploads").exists());
    let id = begin(&manager, 0).await;
    assert_eq!(store.begin_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        store.begun_ids.lock().unwrap().as_slice(),
        std::slice::from_ref(&id)
    );
    manager.abort(&id).await.unwrap();
}

#[tokio::test]
async fn failed_cleanup_retains_upload_slots_until_sweep_succeeds() {
    let f = fixture();
    let store = Arc::new(ControlledStore::new(f.store.clone()));
    store.abort_failures.store(2, Ordering::SeqCst);
    let mut manager = manager_with_store(durable_runtime(f._dir.path()), store.clone());
    manager.policy.max_active = 2;
    manager.policy.max_per_actor = 1;
    let id = begin(&manager, 0).await;
    assert!(manager.abort(&id).await.is_err());
    assert_upload_ceiling(
        manager
            .begin(0, None, "uploader:a".into())
            .await
            .unwrap_err(),
        "per-actor",
        1,
    );
    let other = upload_id(&manager.begin(0, None, "uploader:b".into()).await.unwrap());
    assert_upload_ceiling(
        manager
            .begin(0, None, "uploader:c".into())
            .await
            .unwrap_err(),
        "total",
        2,
    );
    assert!(manager.sweep().await.is_err());
    assert!(staged(&f.root, &id).exists());
    assert_upload_ceiling(
        manager
            .begin(0, None, "uploader:a".into())
            .await
            .unwrap_err(),
        "total",
        2,
    );
    assert_eq!(manager.sweep().await.unwrap(), 1);
    assert_eq!(store.abort_calls.load(Ordering::SeqCst), 3);
    assert!(!staged(&f.root, &id).exists());
    assert!(staged(&f.root, &other).exists());
    let replacement = begin(&manager, 0).await;
    manager.abort(&other).await.unwrap();
    manager.abort(&replacement).await.unwrap();
}

#[tokio::test]
async fn cancelled_begin_keeps_reservation_and_staging_until_expiry_cleanup() {
    let f = fixture();
    let store = Arc::new(ControlledStore::new(f.store.clone()));
    store.pause_after_begin.store(true, Ordering::SeqCst);
    let mut manager = manager_with_store(durable_runtime(f._dir.path()), store.clone());
    manager.policy.max_active = 1;
    manager.policy.max_per_actor = 1;
    let manager = Arc::new(manager);
    let request = tokio::spawn({
        let manager = manager.clone();
        async move { manager.begin(0, None, "uploader:a".into()).await }
    });
    tokio::time::timeout(Duration::from_secs(10), store.begun.notified())
        .await
        .unwrap();
    let id = store.begun_ids.lock().unwrap()[0].clone();
    assert!(staged(&f.root, &id).exists());
    assert!(manager.records.lock().unwrap().is_empty());
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    assert_upload_ceiling(
        tokio::time::timeout(
            Duration::from_secs(10),
            manager.begin(0, None, "uploader:b".into()),
        )
        .await
        .unwrap()
        .unwrap_err(),
        "total",
        1,
    );
    store.pause_after_begin.store(false, Ordering::SeqCst);
    store.release_begin.notify_one();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if manager.records.lock().unwrap().contains_key(&id) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_upload_ceiling(
        manager
            .begin(0, None, "uploader:a".into())
            .await
            .unwrap_err(),
        "total",
        1,
    );
    expire_record(&manager, &id).await;
    assert_eq!(manager.sweep().await.unwrap(), 1);
    assert!(!staged(&f.root, &id).exists());
    let replacement = begin(&manager, 0).await;
    manager.abort(&replacement).await.unwrap();
}
