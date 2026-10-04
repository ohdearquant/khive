use super::*;

#[tokio::test]
async fn poll_preserves_indexed_typed_delivery_rejections() {
    let raw = spaced_delivery_json();
    let mut unsupported = serde_json::to_value(delivery()).unwrap();
    unsupported["protocol_version"] = json!(2);
    let unsupported_raw = serde_json::to_string_pretty(&unsupported).unwrap();
    let mut oversized = serde_json::to_value(delivery()).unwrap();
    oversized["ciphertext"] = json!(encode_base64url(&vec![0; MAX_CIPHERTEXT_BYTES + 1]));
    let oversized_raw = serde_json::to_string_pretty(&oversized).unwrap();
    assert!(matches!(
        Delivery::parse(unsupported_raw.as_bytes()),
        Err(WireDecodeError::Protocol(ProtocolError::UnsupportedVersion))
    ));
    assert!(matches!(
        Delivery::parse(oversized_raw.as_bytes()),
        Err(WireDecodeError::Protocol(ProtocolError::EnvelopeTooLarge))
    ));
    assert!(oversized_raw.len() < crate::request::MAX_REQUEST_BODY_BYTES);
    let server = ScriptedServer::start(vec![
        Reply::json(
            200,
            &poll_body_slices(&[&raw], &[], 5, "2026-09-24T00:00:00Z"),
        ),
        Reply::json(
            200,
            &poll_body_slices(
                &[&unsupported_raw, &oversized_raw, &raw],
                &[],
                71,
                "2026-09-24T00:00:00Z",
            ),
        ),
    ])
    .await
    .unwrap();
    let keys = SpyFacility::new(false);
    let c = client(
        &server,
        false,
        Arc::new(FixedSource(Ok(None))),
        pins(true),
        keys.clone(),
    );
    let guard = c
        .poll(JsonInteger::new(0).unwrap(), PollWait::new(0).unwrap())
        .await
        .unwrap();
    assert_eq!(guard.deliveries.len(), 1);
    assert_eq!(guard.deliveries[0].index(), 0);
    assert!(matches!(
        guard.deliveries[0].opening(),
        DeliveryOpenResult::Opened(_)
    ));
    assert!(guard.rejected_deliveries.is_empty());
    assert!(guard.rejected_receipts.is_empty());
    assert_eq!(keys.opens.load(Ordering::SeqCst), 1);
    let page = c
        .poll(guard.next_cursor, PollWait::new(0).unwrap())
        .await
        .unwrap();
    assert_eq!(page.next_cursor.get(), 71);
    assert!(page.receipts.is_empty());
    assert!(page.rejected_receipts.is_empty());
    assert_eq!(page.deliveries.len(), 1);
    assert_eq!(page.deliveries[0].index(), 2);
    assert_eq!(page.deliveries[0].original_json(), raw.as_bytes());
    assert!(matches!(
        page.deliveries[0].opening(),
        DeliveryOpenResult::Opened(_)
    ));
    assert_eq!(
        page.deliveries[0].receipt_binding().unwrap(),
        receipt().binding
    );
    assert_eq!(page.rejected_deliveries.len(), 2);
    for (rejection, index, code, original) in [
        (
            &page.rejected_deliveries[0],
            0,
            RefusalCode::UnsupportedVersion,
            &unsupported_raw,
        ),
        (
            &page.rejected_deliveries[1],
            1,
            RefusalCode::PayloadTooLarge,
            &oversized_raw,
        ),
    ] {
        assert_eq!(rejection.index, index);
        assert_eq!(rejection.code, code);
        assert_eq!(rejection.original_json(), original.as_bytes());
    }
    assert_eq!(
        keys.opens.load(Ordering::SeqCst),
        2,
        "rejected deliveries must not open"
    );
    assert_eq!(keys.seals.load(Ordering::SeqCst), 0);
    let requests = server.requests().await;
    assert_eq!(
        requests.len(),
        2,
        "polling must not acknowledge rejected deliveries"
    );
    assert!(requests.iter().all(|request| request.method == "GET"));
}

#[tokio::test]
async fn poll_preserves_indexed_receipt_refusals_and_valid_later_signatures() {
    let expected = receipt();
    let first = receipt_item_json(&expected, 1);
    let later = receipt_item_json(&expected, 42);
    let last = receipt_item_json(&expected, 44);
    let malformed = "null";
    let mut invalid_time: Value = serde_json::from_str(&receipt_item_json(&expected, 43)).unwrap();
    invalid_time["recorded_at"] = json!(0);
    let invalid_time_raw = serde_json::to_string_pretty(&invalid_time).unwrap();
    let server = ScriptedServer::start(vec![
        Reply::json(
            200,
            &poll_body_slices(&[], &[&first], 1, "2026-09-24T00:00:00Z"),
        ),
        Reply::json(
            200,
            &poll_body_slices(
                &[],
                &[malformed, &later, &invalid_time_raw, &last],
                91,
                "2026-09-24T00:00:00Z",
            ),
        ),
    ])
    .await
    .unwrap();
    let source = Arc::new(OrderedSource {
        outcomes: std::sync::Mutex::new(vec![Ok(Some(persisted())); 3].into()),
        requested: std::sync::Mutex::new(Vec::new()),
    });
    let keys = SpyFacility::new(true);
    let c = client(&server, true, source.clone(), pins(false), keys.clone());
    let guard = c
        .poll(JsonInteger::new(0).unwrap(), PollWait::new(0).unwrap())
        .await
        .unwrap();
    assert_eq!(guard.receipts.len(), 1);
    assert_eq!(guard.receipts[0].index, 0);
    assert!(matches!(&guard.receipts[0].verification,
        ReceiptVerification::Verified(verified) if verified.receipt() == &expected));
    assert!(guard.rejected_receipts.is_empty());
    let page = c
        .poll(guard.next_cursor, PollWait::new(0).unwrap())
        .await
        .unwrap();
    assert_eq!(page.next_cursor.get(), 91);
    assert!(page.deliveries.is_empty());
    assert!(page.rejected_deliveries.is_empty());
    assert_eq!(page.receipts.len(), 2);
    for (item, index, seq) in [(&page.receipts[0], 1, 42), (&page.receipts[1], 3, 44)] {
        assert_eq!(item.index, index);
        assert_eq!(item.item.seq.get(), seq);
        assert!(matches!(&item.verification,
            ReceiptVerification::Verified(verified) if verified.receipt() == &expected));
    }
    assert_eq!(page.rejected_receipts.len(), 2);
    for (rejection, index, original) in [
        (&page.rejected_receipts[0], 0, malformed.as_bytes()),
        (&page.rejected_receipts[1], 2, invalid_time_raw.as_bytes()),
    ] {
        assert_eq!(rejection.index, index);
        assert_eq!(rejection.code, RefusalCode::InvalidRequest);
        assert_eq!(rejection.original_json(), original);
    }
    assert_eq!(*source.requested.lock().unwrap(), vec![logical(); 3]);
    assert!(source.outcomes.lock().unwrap().is_empty());
    assert_eq!(keys.opens.load(Ordering::SeqCst), 0);
    assert_eq!(keys.seals.load(Ordering::SeqCst), 0);
    let requests = server.requests().await;
    assert_eq!(
        requests.len(),
        2,
        "polling must not acknowledge malformed receipts"
    );
    assert!(requests.iter().all(|request| request.method == "GET"));
}
