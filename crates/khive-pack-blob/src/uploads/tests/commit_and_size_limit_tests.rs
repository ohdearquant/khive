#[tokio::test]
async fn expected_hash_mismatch_aborts_and_cleans_staging() {
    let f = fixture();
    let id = upload_id(
        &f.manager
            .begin(3, Some(content_ref(b"abd")), "uploader:a".into())
            .await
            .unwrap(),
    );
    f.manager.put_part(&id, 0, b"abc".to_vec()).await.unwrap();
    let error = f.manager.commit(&id).await.unwrap_err();
    assert!(error.to_string().contains("content_ref mismatch"));
    assert!(!staged(&f.root, &id).exists());
    assert_unknown(f.manager.commit(&id).await.unwrap_err());
    assert!(!f.store.exists(&content_ref(b"abc")).await.unwrap());
    assert!(!f.store.exists(&content_ref(b"abd")).await.unwrap());
}

#[tokio::test]
async fn partial_commit_refuses_then_remaining_parts_can_complete() {
    let f = fixture();
    let id = begin(&f.manager, 6).await;
    f.manager.put_part(&id, 0, b"abc".to_vec()).await.unwrap();
    let error = f.manager.commit(&id).await.unwrap_err();
    assert!(error.to_string().contains("received 3 bytes, expected 6"));
    assert_eq!(std::fs::read(staged(&f.root, &id)).unwrap(), b"abc");
    f.manager.put_part(&id, 1, b"def".to_vec()).await.unwrap();
    let reference = content_ref(b"abcdef");
    assert_eq!(
        f.manager.commit(&id).await.unwrap(),
        json!({"content_ref": reference.to_string(), "size": 6})
    );
    assert_eq!(
        f.store.get_bounded_verified(&reference, 6).await.unwrap(),
        b"abcdef"
    );
    assert!(!staged(&f.root, &id).exists());
}

#[tokio::test]
async fn part_crossing_declared_size_aborts_without_publishing() {
    let f = fixture();
    let id = begin(&f.manager, 3).await;
    f.manager.put_part(&id, 0, b"ab".to_vec()).await.unwrap();
    let error = f
        .manager
        .put_part(&id, 1, b"cd".to_vec())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("exceeds declared size"));
    assert!(!staged(&f.root, &id).exists());
    assert_unknown(f.manager.commit(&id).await.unwrap_err());
    assert!(!f.store.exists(&content_ref(b"abcd")).await.unwrap());
}

#[tokio::test]
async fn begin_rejects_above_64_mib_before_creating_staging() {
    let f = fixture();
    assert_eq!(MAX_OBJECT_BYTES, 64 * 1024 * 1024);
    let error = f
        .manager
        .begin(MAX_OBJECT_BYTES + 1, None, "uploader:a".into())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("size exceeds"));
    assert!(f.manager.records.lock().unwrap().is_empty());
    assert!(!f.root.join(".uploads").exists());
    let id = begin(&f.manager, MAX_OBJECT_BYTES).await;
    assert_eq!(std::fs::metadata(staged(&f.root, &id)).unwrap().len(), 0);
    f.manager.abort(&id).await.unwrap();
}

#[tokio::test]
async fn advertised_part_limit_matches_live_formula_and_parser_accepts_maximum_part() {
    let f = fixture();
    let budget =
        khive_request::MAX_OPS_INPUT_LEN.min(khive_runtime::daemon::MAX_FRAME_BYTES) as u64;
    assert_eq!(REQUEST_RESERVE, 8192);
    assert!(budget > REQUEST_RESERVE);
    let limit = (budget - REQUEST_RESERVE) * 3 / 4;
    let response = f
        .manager
        .begin(limit, None, "uploader:a".into())
        .await
        .unwrap();
    assert_eq!(response["part_limit"], limit);
    assert_eq!(max_request_part_raw_bytes(), limit);
    let id = upload_id(&response);
    let bytes = vec![b'x'; limit as usize];
    let params = json!({"upload_id": id.to_string(), "index": 0, "bytes": BASE64.encode(&bytes)});
    let ops = serde_json::to_string(&json!({"tool": "blob.put_part", "args": params})).unwrap();
    assert!(ops.len() <= khive_request::MAX_OPS_INPUT_LEN);
    let parsed = khive_request::parse_request(&ops).unwrap();
    assert_eq!(parsed.ops.len(), 1);
    assert_eq!(parsed.ops[0].tool, "blob.put_part");
    assert!(
        serde_json::to_vec(&json!({"ops": ops, "actor_id": "uploader:a", "namespace": "local"}))
            .unwrap()
            .len()
            <= khive_runtime::daemon::MAX_FRAME_BYTES
    );
    let decoded = Value::Object(
        parsed.ops[0]
            .args
            .iter()
            .map(|(name, argument)| (name.clone(), argument.as_value().unwrap().clone()))
            .collect(),
    );
    assert_eq!(
        handle_put_part(&f.manager, decoded).await.unwrap(),
        json!({"next_index": 1, "received_bytes": limit})
    );
    assert_eq!(
        f.manager.commit(&id).await.unwrap()["content_ref"],
        content_ref(&bytes).to_string()
    );
}

#[tokio::test]
async fn part_limit_plus_one_is_rejected_on_decoded_length_without_aborting() {
    let f = fixture();
    let limit = max_request_part_raw_bytes();
    let id = begin(&f.manager, limit + 1).await;
    let params = json!({"upload_id": id.to_string(), "index": 0, "bytes": BASE64.encode(vec![b'x'; limit as usize + 1])});
    let error = handle_put_part(&f.manager, params).await.unwrap_err();
    assert!(
        error.to_string().contains(&format!(
            "decoded length {} exceeds part_limit {limit}",
            limit + 1
        )),
        "{error}"
    );
    assert_eq!(std::fs::metadata(staged(&f.root, &id)).unwrap().len(), 0);
    let entry = f.manager.record(&id).unwrap();
    let record = entry.lock().await;
    assert_eq!(record.received_bytes, 0);
    assert_eq!(record.next_index, 0);
    assert!(record.phase == UploadPhase::Active);
    drop(record);
    f.manager.abort(&id).await.unwrap();
}
