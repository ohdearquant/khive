fn parsed_part_params(id: &UploadId, index: u64, bytes: &[u8]) -> Value {
    let params =
        json!({"upload_id": id.to_string(), "index": index, "bytes": BASE64.encode(bytes)});
    let ops = serde_json::to_string(&json!({"tool": "blob.put_part", "args": params})).unwrap();
    assert!(ops.len() < khive_request::MAX_OPS_INPUT_LEN);
    let parsed = khive_request::parse_request(&ops).unwrap();
    assert_eq!(parsed.ops.len(), 1);
    assert_eq!(parsed.ops[0].tool, "blob.put_part");
    Value::Object(
        parsed.ops[0]
            .args
            .iter()
            .map(|(name, argument)| (name.clone(), argument.as_value().unwrap().clone()))
            .collect(),
    )
}

#[tokio::test]
async fn oversized_tail_retry_aborts_for_decoded_and_predecode_limits() {
    let limit = max_request_part_raw_bytes();
    for excess in [1, 6] {
        let f = fixture();
        let id = begin(&f.manager, limit * 2).await;
        f.manager.put_part(&id, 0, b"abc".to_vec()).await.unwrap();
        let params = parsed_part_params(&id, 0, &vec![b'x'; (limit + excess) as usize]);
        assert_eq!(
            params["bytes"].as_str().unwrap().len() as u64 > limit * 4 / 3 + 4,
            excess == 6
        );
        let error = handle_put_part(&f.manager, params).await.unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error}");
        assert!(
            error.to_string().contains(if excess == 1 {
                "tail retry differs"
            } else {
                "base64 input exceeds"
            }),
            "{error}"
        );
        assert!(!staged(&f.root, &id).exists(), "excess {excess}");
        assert_unknown(f.manager.commit(&id).await.unwrap_err());
        assert!(!f.store.exists(&content_ref(b"abc")).await.unwrap());
    }
}

#[tokio::test]
async fn oversized_next_part_crossing_declared_size_aborts_for_both_limits() {
    let limit = max_request_part_raw_bytes();
    for excess in [1, 6] {
        let f = fixture();
        let id = begin(&f.manager, limit + 6).await;
        f.manager
            .put_part(&id, 0, b"prefix".to_vec())
            .await
            .unwrap();
        let params = parsed_part_params(&id, 1, &vec![b'x'; (limit + excess) as usize]);
        assert_eq!(
            params["bytes"].as_str().unwrap().len() as u64 > limit * 4 / 3 + 4,
            excess == 6
        );
        let error = handle_put_part(&f.manager, params).await.unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error}");
        assert!(
            error.to_string().contains(if excess == 1 {
                "exceeds declared size"
            } else {
                "base64 input exceeds"
            }),
            "{error}"
        );
        assert!(!staged(&f.root, &id).exists(), "excess {excess}");
        assert_unknown(f.manager.commit(&id).await.unwrap_err());
        assert!(!f.store.exists(&content_ref(b"prefix")).await.unwrap());
    }
    for (excess, padding) in [(6, 0), (7, 2), (8, 1)] {
        let f = fixture();
        let raw_length = limit + excess;
        let id = begin(&f.manager, raw_length - 1).await;
        let params = parsed_part_params(&id, 0, &vec![b'x'; raw_length as usize]);
        let encoded = params["bytes"].as_str().unwrap();
        assert!(encoded.len() as u64 > limit * 4 / 3 + 4);
        assert_eq!(
            encoded
                .bytes()
                .rev()
                .take_while(|byte| *byte == b'=')
                .count(),
            padding
        );
        let error = handle_put_part(&f.manager, params).await.unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error}");
        assert!(
            error.to_string().contains("base64 input exceeds"),
            "{error}"
        );
        assert!(
            !staged(&f.root, &id).exists(),
            "raw excess {excess}, padding {padding}"
        );
        assert_unknown(f.manager.commit(&id).await.unwrap_err());
    }
}

#[tokio::test]
async fn oversized_first_part_within_declared_size_keeps_upload_usable_for_both_limits() {
    let limit = max_request_part_raw_bytes();
    for excess in [1, 6] {
        let f = fixture();
        let id = begin(&f.manager, limit + 6).await;
        let params = parsed_part_params(&id, 0, &vec![b'x'; (limit + excess) as usize]);
        assert_eq!(
            params["bytes"].as_str().unwrap().len() as u64 > limit * 4 / 3 + 4,
            excess == 6
        );
        let error = handle_put_part(&f.manager, params).await.unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error}");
        assert!(
            error.to_string().contains(if excess == 1 {
                "decoded length"
            } else {
                "base64 input exceeds"
            }),
            "{error}"
        );
        assert_eq!(std::fs::metadata(staged(&f.root, &id)).unwrap().len(), 0);
        {
            let entry = f.manager.record(&id).unwrap();
            let record = entry.lock().await;
            assert_eq!(record.received_bytes, 0);
            assert_eq!(record.next_index, 0);
            assert_eq!(record.hasher.finalize(), blake3::hash(b""));
            assert!(record.phase == UploadPhase::Active);
        }
        assert_eq!(
            handle_put_part(&f.manager, parsed_part_params(&id, 0, b"a"))
                .await
                .unwrap(),
            json!({"next_index": 1, "received_bytes": 1})
        );
        assert_eq!(
            handle_put_part(&f.manager, parsed_part_params(&id, 1, b"bc"))
                .await
                .unwrap(),
            json!({"next_index": 2, "received_bytes": 3})
        );
        assert_eq!(std::fs::read(staged(&f.root, &id)).unwrap(), b"abc");
        assert_eq!(
            f.manager.abort(&id).await.unwrap(),
            json!({"aborted": true})
        );
        assert!(!staged(&f.root, &id).exists());
    }
    for malformed in ["inner padding", "noncanonical terminal padding bits"] {
        let f = fixture();
        let id = begin(&f.manager, 3).await;
        let mut encoded = BASE64.encode(vec![b'x'; (limit + 7) as usize]);
        assert!(encoded.len() as u64 > limit * 4 / 3 + 4);
        if malformed == "inner padding" {
            encoded.replace_range(4..5, "=");
        } else {
            assert!(encoded.ends_with("eA=="));
            let offset = encoded.len() - 3;
            encoded.replace_range(offset..offset + 1, "B");
        }
        let ops = serde_json::to_string(&json!({
            "tool": "blob.put_part",
            "args": {"upload_id": id.to_string(), "index": 0, "bytes": encoded}
        }))
        .unwrap();
        assert!(ops.len() < khive_request::MAX_OPS_INPUT_LEN);
        let parsed = khive_request::parse_request(&ops).unwrap();
        let params = Value::Object(
            parsed.ops[0]
                .args
                .iter()
                .map(|(name, argument)| (name.clone(), argument.as_value().unwrap().clone()))
                .collect(),
        );
        let error = handle_put_part(&f.manager, params).await.unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error}");
        assert!(
            error.to_string().contains("invalid base64"),
            "{malformed}: {error}"
        );
        assert_eq!(std::fs::metadata(staged(&f.root, &id)).unwrap().len(), 0);
        {
            let entry = f.manager.record(&id).unwrap();
            let record = entry.lock().await;
            assert_eq!(record.received_bytes, 0);
            assert_eq!(record.next_index, 0);
            assert_eq!(record.hasher.finalize(), blake3::hash(b""));
            assert!(record.phase == UploadPhase::Active);
        }
        handle_put_part(&f.manager, parsed_part_params(&id, 0, b"a"))
            .await
            .unwrap();
        handle_put_part(&f.manager, parsed_part_params(&id, 1, b"bc"))
            .await
            .unwrap();
        assert_eq!(
            f.manager.commit(&id).await.unwrap(),
            json!({"content_ref": content_ref(b"abc").to_string(), "size": 3})
        );
        assert!(!staged(&f.root, &id).exists());
        assert_eq!(
            f.store
                .get_bounded_verified(&content_ref(b"abc"), 3)
                .await
                .unwrap(),
            b"abc"
        );
    }
}
