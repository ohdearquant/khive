#[tokio::test]
async fn wrong_index_refuses_without_changing_upload() {
    let f = fixture();
    let id = begin(&f.manager, 3).await;
    let error = f.manager.put_part(&id, 1, b"a".to_vec()).await.unwrap_err();
    assert!(error.to_string().contains("expected index 0"));
    assert_eq!(std::fs::read(staged(&f.root, &id)).unwrap(), b"");
    f.manager.put_part(&id, 0, b"a".to_vec()).await.unwrap();
    f.manager.put_part(&id, 1, b"b".to_vec()).await.unwrap();
    let error = f.manager.put_part(&id, 0, b"a".to_vec()).await.unwrap_err();
    assert!(error.to_string().contains("expected index 2"));
    assert_eq!(std::fs::read(staged(&f.root, &id)).unwrap(), b"ab");
    f.manager.put_part(&id, 2, b"c".to_vec()).await.unwrap();
    let result = f.manager.commit(&id).await.unwrap();
    assert_eq!(result["content_ref"], content_ref(b"abc").to_string());
}

#[tokio::test]
async fn identical_tail_retry_preserves_hash_counts_and_advances_idle_clocks() {
    let f = fixture();
    let id = begin(&f.manager, 6).await;
    let accepted = f.manager.put_part(&id, 0, b"abc".to_vec()).await.unwrap();
    let entry = f.manager.record(&id).unwrap();
    let (last_part, hash, tail) = {
        let mut record = entry.lock().await;
        record.last_part = Instant::now().checked_sub(Duration::from_secs(10)).unwrap();
        (record.last_part, record.hasher.finalize(), record.tail)
    };
    let path = staged(&f.root, &id);
    age_file(&path);
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    let retried = f.manager.put_part(&id, 0, b"abc".to_vec()).await.unwrap();
    assert_eq!(retried, accepted);
    {
        let record = entry.lock().await;
        assert_eq!(record.received_bytes, 3);
        assert_eq!(record.next_index, 1);
        assert_eq!(record.hasher.finalize(), hash);
        assert_eq!(record.tail, tail);
        assert!(record.last_part > last_part);
        assert_eq!(lease(&f.root, &id)["renew_seq"], 2);
    }
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        modified
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"abc");
    f.manager.put_part(&id, 1, b"def".to_vec()).await.unwrap();
    let result = f.manager.commit(&id).await.unwrap();
    assert_eq!(result["content_ref"], content_ref(b"abcdef").to_string());
}

#[tokio::test]
async fn different_length_tail_retry_aborts_and_commit_is_unknown() {
    let f = fixture();
    let id = begin(&f.manager, 6).await;
    f.manager.put_part(&id, 0, b"abc".to_vec()).await.unwrap();
    let error = f
        .manager
        .put_part(&id, 0, b"ab".to_vec())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("tail retry differs"));
    assert!(!staged(&f.root, &id).exists());
    assert_unknown(f.manager.commit(&id).await.unwrap_err());
    assert!(!f.store.exists(&content_ref(b"abc")).await.unwrap());
}

#[tokio::test]
async fn same_length_different_bytes_tail_retry_aborts_and_commit_is_unknown() {
    let f = fixture();
    let id = begin(&f.manager, 6).await;
    f.manager.put_part(&id, 0, b"abc".to_vec()).await.unwrap();
    let error = f
        .manager
        .put_part(&id, 0, b"abd".to_vec())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("tail retry differs"));
    assert!(!staged(&f.root, &id).exists());
    assert_unknown(f.manager.commit(&id).await.unwrap_err());
    assert!(!f.store.exists(&content_ref(b"abc")).await.unwrap());
}

#[tokio::test]
async fn zero_byte_part_advances_index_and_is_retryable() {
    let f = fixture();
    let id = begin(&f.manager, 1).await;
    let params = json!({"upload_id": id.to_string(), "index": 0, "bytes": ""});
    let accepted = handle_put_part(&f.manager, params.clone()).await.unwrap();
    assert_eq!(accepted, json!({"next_index": 1, "received_bytes": 0}));
    assert_eq!(handle_put_part(&f.manager, params).await.unwrap(), accepted);
    f.manager.put_part(&id, 1, b"a".to_vec()).await.unwrap();
    assert_eq!(
        f.manager.commit(&id).await.unwrap()["content_ref"],
        content_ref(b"a").to_string()
    );
}

#[tokio::test]
async fn zero_byte_object_commits_without_a_part() {
    let f = fixture();
    let id = begin(&f.manager, 0).await;
    let result = f.manager.commit(&id).await.unwrap();
    let reference = content_ref(b"");
    assert_eq!(
        result,
        json!({"content_ref": reference.to_string(), "size": 0})
    );
    assert_eq!(
        f.store.get_bounded_verified(&reference, 0).await.unwrap(),
        b""
    );
    assert!(!staged(&f.root, &id).exists());
    assert_unknown(f.manager.commit(&id).await.unwrap_err());
}
