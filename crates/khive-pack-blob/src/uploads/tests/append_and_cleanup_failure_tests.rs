#[tokio::test]
async fn failed_abort_is_retained_and_retried_by_successive_sweeps() {
    let f = fixture();
    let store = Arc::new(ControlledStore::new(f.store.clone()));
    store.abort_failures.store(2, Ordering::SeqCst);
    let manager = manager_with_store(durable_runtime(f._dir.path()), store.clone());
    let id = begin(&manager, 1).await;
    assert!(manager
        .abort(&id)
        .await
        .unwrap_err()
        .to_string()
        .contains("injected abort failure"));
    assert!(manager.record(&id).unwrap().lock().await.phase == UploadPhase::Aborted);
    assert!(staged(&f.root, &id).exists());
    assert!(manager
        .sweep()
        .await
        .unwrap_err()
        .to_string()
        .contains("injected abort failure"));
    assert!(manager.records.lock().unwrap().contains_key(&id));
    assert!(staged(&f.root, &id).exists());
    assert_eq!(manager.sweep().await.unwrap(), 1);
    assert_eq!(store.abort_calls.load(Ordering::SeqCst), 3);
    assert!(!manager.records.lock().unwrap().contains_key(&id));
    assert!(!staged(&f.root, &id).exists());
}

#[tokio::test]
async fn failed_error_cleanup_retains_aborted_upload_until_sweep_retries() {
    let f = fixture();
    let store = Arc::new(ControlledStore::new(f.store.clone()));
    store.abort_failures.store(1, Ordering::SeqCst);
    let manager = manager_with_store(durable_runtime(f._dir.path()), store.clone());
    let id = begin(&manager, 3).await;
    manager.put_part(&id, 0, b"abc".to_vec()).await.unwrap();
    let error = manager.put_part(&id, 0, b"abd".to_vec()).await.unwrap_err();
    assert!(error.to_string().contains("tail retry differs"));
    assert!(manager.record(&id).unwrap().lock().await.phase == UploadPhase::Aborted);
    assert_eq!(std::fs::read(staged(&f.root, &id)).unwrap(), b"abc");
    assert_eq!(manager.sweep().await.unwrap(), 1);
    assert_eq!(store.abort_calls.load(Ordering::SeqCst), 2);
    assert!(!staged(&f.root, &id).exists());
    assert_unknown(manager.commit(&id).await.unwrap_err());
}

#[tokio::test]
async fn append_failure_after_real_write_aborts_and_never_acknowledges() {
    let f = fixture();
    let store = Arc::new(ControlledStore::new(f.store.clone()));
    store.fail_after_append.store(true, Ordering::SeqCst);
    let manager = manager_with_store(durable_runtime(f._dir.path()), store);
    let id = begin(&manager, 3).await;
    let error = manager.put_part(&id, 0, b"abc".to_vec()).await.unwrap_err();
    assert!(error
        .to_string()
        .contains("injected append failure after write"));
    assert!(!staged(&f.root, &id).exists());
    assert_unknown(manager.commit(&id).await.unwrap_err());
    assert!(!f.store.exists(&content_ref(b"abc")).await.unwrap());
}

#[tokio::test]
async fn inconsistent_backend_length_aborts_and_never_acknowledges() {
    let f = fixture();
    let store = Arc::new(ControlledStore::new(f.store.clone()));
    store.wrong_append_length.store(true, Ordering::SeqCst);
    let manager = manager_with_store(durable_runtime(f._dir.path()), store);
    let id = begin(&manager, 3).await;
    let error = manager.put_part(&id, 0, b"abc".to_vec()).await.unwrap_err();
    assert!(error.to_string().contains("staged upload length differs"));
    assert!(!staged(&f.root, &id).exists());
    assert_unknown(manager.commit(&id).await.unwrap_err());
}

#[tokio::test]
async fn cancelled_append_never_acknowledges_inconsistent_staged_bytes() {
    let f = fixture();
    let store = Arc::new(ControlledStore::new(f.store.clone()));
    store.pause_after_append.store(true, Ordering::SeqCst);
    let manager = Arc::new(manager_with_store(
        durable_runtime(f._dir.path()),
        store.clone(),
    ));
    let id = begin(&manager, 6).await;
    let task = tokio::spawn({
        let manager = manager.clone();
        let id = id.clone();
        async move { manager.put_part(&id, 0, b"abc".to_vec()).await }
    });
    tokio::time::timeout(Duration::from_secs(10), store.appended.notified())
        .await
        .unwrap();
    assert_eq!(std::fs::read(staged(&f.root, &id)).unwrap(), b"abc");
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    {
        let entry = manager.record(&id).unwrap();
        let record = entry.lock().await;
        assert!(record.phase == UploadPhase::InFlight);
        assert_eq!(record.received_bytes, 0);
        assert_eq!(record.next_index, 0);
        assert_eq!(record.hasher.finalize(), blake3::hash(b""));
    }
    store.pause_after_append.store(false, Ordering::SeqCst);
    assert_unknown(manager.put_part(&id, 0, b"abc".to_vec()).await.unwrap_err());
    assert!(!staged(&f.root, &id).exists());
    assert_unknown(manager.commit(&id).await.unwrap_err());
    assert!(!f.store.exists(&content_ref(b"abcabc")).await.unwrap());
}

#[tokio::test]
async fn failed_or_cancelled_tail_renewal_never_acknowledges_or_advances_acceptance() {
    for cancel in [false, true] {
        let f = fixture();
        let store = Arc::new(ControlledStore::new(f.store.clone()));
        let manager = Arc::new(manager_with_store(
            durable_runtime(f._dir.path()),
            store.clone(),
        ));
        let id = begin(&manager, 3).await;
        manager.put_part(&id, 0, b"abc".to_vec()).await.unwrap();
        let record = manager.record(&id).unwrap();
        let clock = record.lock().await.last_part;
        store.abort_failures.store(1, Ordering::SeqCst);
        store.pause_renew.store(cancel, Ordering::SeqCst);
        store.fail_renew.store(!cancel, Ordering::SeqCst);
        let task = tokio::spawn({
            let manager = manager.clone();
            let id = id.clone();
            async move { manager.put_part(&id, 0, b"abc".to_vec()).await }
        });
        if cancel {
            tokio::time::timeout(Duration::from_secs(10), store.renewing.notified())
                .await
                .unwrap();
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            assert!(record.lock().await.phase == UploadPhase::InFlight);
            assert_unknown(manager.put_part(&id, 0, b"abc".to_vec()).await.unwrap_err());
        } else {
            assert!(task
                .await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("injected renewal failure"));
        }
        let record = record.lock().await;
        assert!(record.phase == UploadPhase::Aborted);
        assert_eq!(
            (record.received_bytes, record.next_index, record.last_part),
            (3, 1, clock)
        );
        assert_eq!(record.hasher.finalize(), blake3::hash(b"abc"));
        assert_eq!(lease(&f.root, &id)["renew_seq"], 1);
        assert_eq!(std::fs::read(staged(&f.root, &id)).unwrap(), b"abc");
        drop(record);
        assert_eq!(manager.sweep().await.unwrap(), 1);
        assert!(
            !staged(&f.root, &id).exists()
                && !staged(&f.root, &id).with_extension("lease").exists()
        );
        assert!(!f.store.exists(&content_ref(b"abc")).await.unwrap());
    }
}
