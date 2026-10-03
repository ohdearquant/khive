use super::*;
use std::collections::BTreeMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

struct KeyedSource {
    records: BTreeMap<Uuid, PersistedSubmission>,
    requested: std::sync::Mutex<Vec<Uuid>>,
}

#[async_trait]
impl OutboundSource for KeyedSource {
    async fn current(&self, logical: Uuid) -> Result<Option<PersistedSubmission>, SourceError> {
        self.requested.lock().unwrap().push(logical);
        Ok(self.records.get(&logical).cloned())
    }
}

fn keyed_source(records: Vec<PersistedSubmission>) -> Arc<KeyedSource> {
    Arc::new(KeyedSource {
        records: records
            .into_iter()
            .map(|record| (record.logical_message_id.into_uuid(), record))
            .collect(),
        requested: std::sync::Mutex::new(Vec::new()),
    })
}

fn status_body(logical: Uuid, state: &str, signed: Option<&WireReceipt>) -> String {
    json!({
        "logical_message_id": logical,
        "state": state,
        "delivery_attempt_id": signed.map(|r| r.binding.delivery_attempt_id),
        "receipt": signed
    })
    .to_string()
}

#[tokio::test]
async fn status_cannot_finalize_a_with_b_genuine_receipt() {
    let a = persisted();
    let mut b = a.clone();
    b.logical_message_id = other_id();
    let mut binding_b = receipt().binding;
    binding_b.logical_message_id = b.logical_message_id;
    let receipt_b = WireReceipt::sign(binding_b, ReceiptDisposition::Stored, &facility(false))
        .expect("a genuine signed receipt for B");
    receipt_b
        .verify(&facility(false).public_keys().signing)
        .expect("B's signature must verify under the confirmed recipient key");
    let server = ScriptedServer::start(vec![
        Reply::json(200, &status_body(logical(), "pending", None)),
        Reply::json(
            200,
            &status_body(
                b.logical_message_id.into_uuid(),
                "recipient_stored",
                Some(&receipt_b),
            ),
        ),
        Reply::json(
            200,
            &status_body(logical(), "recipient_stored", Some(&receipt_b)),
        ),
    ])
    .await
    .unwrap();
    let source = keyed_source(vec![a, b.clone()]);
    let c = client(
        &server,
        true,
        source.clone(),
        pins(false),
        SpyFacility::new(true),
    );
    let pending_a = c.status(logical()).await.unwrap();
    assert_eq!(pending_a.state, MessageState::Pending);
    assert!(pending_a.receipt.is_none());
    let stored_b = c.status(b.logical_message_id.into_uuid()).await.unwrap();
    assert!(matches!(&stored_b.receipt,
        Some(ReceiptVerification::Verified(verified)) if verified.receipt() == &receipt_b));
    assert_eq!(
        *source.requested.lock().unwrap(),
        vec![b.logical_message_id.into_uuid()]
    );
    let wrong = c.status(logical()).await.unwrap();
    assert_eq!(wrong.logical_message_id.into_uuid(), logical());
    assert!(
        matches!(
            wrong.receipt,
            Some(ReceiptVerification::Rejected(
                ReceiptRejection::BindingMismatch
            ))
        ),
        "status(A) must reject B's genuine receipt instead of finalizing A"
    );
    let requests = server.requests().await;
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[2].target,
        format!("/node/v1/messages/{}", logical())
    );
    assert!(requests.iter().all(|request| request.method == "GET"));
}

#[tokio::test]
async fn non_json_400_and_413_are_permanent_for_submit_and_ack() {
    let cases = [
        (400, "<html>invalid request</html>", true),
        (400, "", true),
        (413, "<html>request entity too large</html>", true),
        (413, "", true),
        (408, "<html>request timeout</html>", false),
        (429, "<html>rate limited</html>", false),
    ];
    let mut replies = vec![
        Reply::json(202, &admission()),
        Reply::json(200, "{\"recorded\":true}"),
    ];
    let edge_reply = |status, body: &str| Reply {
        status,
        headers: vec![("Content-Type".into(), "text/html".into())],
        body: body.as_bytes().to_vec(),
        gate: None,
    };
    for (status, body, _) in cases {
        replies.push(edge_reply(status, body));
        replies.push(edge_reply(status, body));
    }
    let server = ScriptedServer::start(replies).await.unwrap();
    let sender = ordinary_client(&server, true);
    let recipient = ordinary_client(&server, false);
    assert!(matches!(
        sender.submit(logical()).await.unwrap(),
        SendOutcome::Pending(PendingDetail::Admitted { .. })
    ));
    recipient
        .ack(&receipt())
        .await
        .expect("a real acknowledgement succeeds first");
    for (status, body, permanent) in cases {
        for error in [
            sender
                .submit(logical())
                .await
                .expect_err("the edge refuses submit"),
            recipient
                .ack(&receipt())
                .await
                .expect_err("the edge refuses acknowledgement"),
        ] {
            if permanent {
                assert!(
                    matches!(
                        error.channel_error(),
                        Some(ChannelError::PermanentTransport(_))
                    ),
                    "a non-protocol {status} body {body:?} must be permanent, not retried: {error}"
                );
            } else {
                assert!(
                    matches!(error.channel_error(), Some(ChannelError::Transport(_))),
                    "a non-protocol {status} body {body:?} must remain retryable: {error}"
                );
            }
        }
    }
    let requests = server.requests().await;
    assert_eq!(requests.len(), 14);
    for pair in requests.chunks_exact(2) {
        assert_eq!(pair[0].target, "/node/v1/messages");
        assert_eq!(pair[1].target, "/node/v1/receipts");
        assert!(pair.iter().all(|request| request.method == "POST"));
    }
}

fn lexical_delivery_cases() -> [String; 2] {
    let raw = serde_json::to_string(&delivery()).unwrap();
    [
        raw.replacen(
            &format!(
                "\"delivery_attempt_id\":\"{}\"",
                delivery().delivery_attempt_id
            ),
            r#""delivery_attempt_id":"\ud800""#,
            1,
        ),
        raw.replacen("\"protocol_version\":1", "\"protocol_version\":1e400", 1),
    ]
}

#[tokio::test]
async fn lexical_delivery_failures_preserve_later_items_cursor_and_raw_rejection() {
    let valid = spaced_delivery_json();
    let signed = receipt();
    for malformed in lexical_delivery_cases() {
        assert!(
            serde_json::from_str::<Value>(&malformed).is_err(),
            "the fixture must fail JSON item decoding"
        );
        let first_receipt = receipt_item_json(&signed, 1);
        let later_receipt = receipt_item_json(&signed, 2);
        let server = ScriptedServer::start(vec![
            Reply::json(
                200,
                &poll_body_slices(&[&valid], &[&first_receipt], 1, "2026-09-24T00:00:00Z"),
            ),
            Reply::json(
                200,
                &poll_body_slices(
                    &[&malformed, &valid],
                    &[&later_receipt],
                    2,
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
        assert!(matches!(
            guard.deliveries[0].opening(),
            DeliveryOpenResult::Opened(_)
        ));
        assert_eq!(guard.receipts.len(), 1);
        let page = c
            .poll(guard.next_cursor, PollWait::new(0).unwrap())
            .await
            .expect("a lexical failure in one delivery must not discard its page");
        assert_eq!(page.next_cursor.get(), 2);
        assert_eq!(page.deliveries.len(), 1);
        assert_eq!(page.deliveries[0].index(), 1);
        assert!(matches!(
            page.deliveries[0].opening(),
            DeliveryOpenResult::Opened(_)
        ));
        assert_eq!(page.deliveries[0].original_json(), valid.as_bytes());
        assert_eq!(page.receipts.len(), 1);
        assert_eq!(page.receipts[0].item.seq.get(), 2);
        assert_eq!(page.rejected_deliveries.len(), 1);
        let rejection = &page.rejected_deliveries[0];
        assert_eq!(rejection.index, 0);
        assert_eq!(rejection.code, RefusalCode::InvalidRequest);
        assert_eq!(rejection.original_json(), malformed.as_bytes());
        assert!(page.rejected_receipts.is_empty());
        assert_eq!(keys.opens.load(Ordering::SeqCst), 2);
        assert_eq!(keys.seals.load(Ordering::SeqCst), 0);
        let requests = server.requests().await;
        assert_eq!(
            requests.len(),
            2,
            "poll must never acknowledge a rejected item"
        );
        assert!(requests.iter().all(|request| request.method == "GET"));
    }
}

#[tokio::test]
async fn lexical_receipt_failures_preserve_later_verified_receipt_and_cursor() {
    let signed = receipt();
    let first = receipt_item_json(&signed, 1);
    let valid = receipt_item_json(&signed, 2);
    let malformed = [
        first.replacen(
            "\"recorded_at\":\"2026-09-24T00:00:00Z\"",
            r#""recorded_at":"\ud800""#,
            1,
        ),
        first.replacen("\"seq\":1", "\"seq\":1e400", 1),
    ];
    let delivery_raw = spaced_delivery_json();
    for bad in malformed {
        assert!(
            serde_json::from_str::<Value>(&bad).is_err(),
            "the fixture must fail JSON item decoding"
        );
        let server = ScriptedServer::start(vec![
            Reply::json(
                200,
                &poll_body_slices(&[&delivery_raw], &[&first], 1, "2026-09-24T00:00:00Z"),
            ),
            Reply::json(
                200,
                &poll_body_slices(&[&delivery_raw], &[&bad, &valid], 2, "2026-09-24T00:00:00Z"),
            ),
        ])
        .await
        .unwrap();
        let source = keyed_source(vec![persisted()]);
        let keys = SpyFacility::new(true);
        let c = client(&server, true, source.clone(), pins(false), keys.clone());
        let guard = c
            .poll(JsonInteger::new(0).unwrap(), PollWait::new(0).unwrap())
            .await
            .unwrap();
        assert!(
            matches!(&guard.receipts[0].verification, ReceiptVerification::Verified(v) if v.receipt() == &signed)
        );
        let page = c
            .poll(guard.next_cursor, PollWait::new(0).unwrap())
            .await
            .expect("a lexical failure in one receipt must not discard its page");
        assert_eq!(page.next_cursor.get(), 2);
        assert_eq!(page.receipts.len(), 1);
        assert_eq!(page.receipts[0].index, 1);
        assert_eq!(page.receipts[0].item.seq.get(), 2);
        assert!(
            matches!(&page.receipts[0].verification, ReceiptVerification::Verified(v) if v.receipt() == &signed)
        );
        assert_eq!(page.deliveries.len(), 1);
        assert_eq!(page.deliveries[0].original_json(), delivery_raw.as_bytes());
        assert_eq!(page.rejected_receipts.len(), 1);
        let rejection = &page.rejected_receipts[0];
        assert_eq!(rejection.index, 0);
        assert_eq!(rejection.code, RefusalCode::InvalidRequest);
        assert_eq!(rejection.original_json(), bad.as_bytes());
        assert!(page.rejected_deliveries.is_empty());
        assert_eq!(*source.requested.lock().unwrap(), vec![logical(); 2]);
        assert_eq!(keys.opens.load(Ordering::SeqCst), 0);
        assert_eq!(keys.seals.load(Ordering::SeqCst), 0);
        let requests = server.requests().await;
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|request| request.method == "GET"));
    }
}

#[test]
fn production_constructor_refuses_non_origin_urls() {
    let build = |url| {
        NodeClient::new(
            binding(true),
            url,
            keyed_source(vec![persisted()]),
            pins(false),
            SpyFacility::new(true),
        )
    };
    assert!(
        build("https://relay.example/").is_ok(),
        "an HTTPS origin remains accepted"
    );
    for url in [
        "http://relay.example/",
        "http://127.0.0.1/",
        "https://user@relay.example/",
        "https://user:password@relay.example/",
        "https://relay.example/?",
        "https://relay.example/?x=1",
        "https://relay.example/#",
        "https://relay.example/#fragment",
        "https://relay.example/prefix",
    ] {
        let error = match build(url) {
            Ok(_) => panic!("production constructor accepted {url}"),
            Err(error) => error,
        };
        assert!(
            matches!(error.channel_error(), Some(ChannelError::InvalidEnvelope(message)) if message == "service must be an HTTPS origin without credentials"),
            "{url}: {error}"
        );
    }
}

struct ChunkedServer {
    url: String,
    worker: JoinHandle<Result<String, String>>,
}

impl ChunkedServer {
    async fn start(body: Vec<u8>) -> Self {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let serve = async move {
            let (mut stream, _) = listener.accept().await.map_err(|e| e.to_string())?;
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let remaining = 16_384_usize.saturating_sub(request.len());
                if remaining == 0 {
                    return Err("chunked fixture request headers exceed their bound".into());
                }
                let count = stream
                    .read(&mut buffer[..remaining.min(1024)])
                    .await
                    .map_err(|e| e.to_string())?;
                if count == 0 {
                    return Err("chunked fixture request ended before its headers".into());
                }
                request.extend_from_slice(&buffer[..count]);
            }
            let request = String::from_utf8(request).map_err(|e| e.to_string())?;
            let line = request
                .lines()
                .next()
                .ok_or("missing request line")?
                .to_owned();
            if !line.starts_with("GET ") {
                return Err("chunked fixture requires a body-free GET".into());
            }
            // There is deliberately no Content-Length: only the streaming cap can refuse.
            stream.write_all(b"HTTP/1.1 200 Fixture\r\nConnection: close\r\nTransfer-Encoding: chunked\r\nContent-Type: application/json\r\n\r\n")
                .await.map_err(|e| e.to_string())?;
            for chunk in body.chunks(32_768) {
                let mut frame = format!("{:x}\r\n", chunk.len()).into_bytes();
                frame.extend_from_slice(chunk);
                frame.extend_from_slice(b"\r\n");
                stream.write_all(&frame).await.map_err(|e| e.to_string())?;
            }
            // A capped client may close after the last data chunk, before this terminator.
            let _ = stream.write_all(b"0\r\n\r\n").await;
            Ok(line)
        };
        let worker = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(10), serve)
                .await
                .map_err(|_| "chunked fixture timed out".to_owned())?
        });
        Self {
            url: format!("http://{address}"),
            worker,
        }
    }
}

impl Drop for ChunkedServer {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

#[tokio::test]
async fn chunked_response_enforces_the_two_mib_streaming_limit() {
    for size in [2_097_152, 2_097_153] {
        let json = status_json(None);
        let body = format!("{}{}", " ".repeat(size - json.len()), json).into_bytes();
        assert_eq!(body.len(), size);
        assert!(serde_json::from_slice::<StatusResponse>(&body).is_ok());
        let mut server = ChunkedServer::start(body).await;
        let c = NodeClient::test_http(
            binding(true),
            &server.url,
            keyed_source(vec![persisted()]),
            pins(false),
            SpyFacility::new(true),
        )
        .unwrap();
        let result = c.status(logical()).await;
        if size == 2_097_152 {
            assert_eq!(result.unwrap().state, MessageState::Unknown);
        } else {
            let error = result.expect_err("a chunked body one byte above 2 MiB must be refused");
            assert!(
                matches!(error.channel_error(), Some(ChannelError::Transport(message)) if message == "node response exceeds local limit"),
                "the streaming cap, rather than a parser/network failure, must refuse: {error}"
            );
        }
        let request_line = (&mut server.worker).await.unwrap().unwrap();
        assert_eq!(
            request_line,
            format!("GET /node/v1/messages/{} HTTP/1.1", logical())
        );
    }
}

#[tokio::test]
async fn receipt_pin_identity_and_fingerprint_must_match_persisted_owner() {
    let signed = receipt();
    signed
        .verify(&facility(false).public_keys().signing)
        .unwrap();
    for mismatch in [false, true] {
        let mut record = persisted();
        let keys = facility(false).public_keys();
        let mut owner = identity(false);
        if mismatch {
            record.recipient_fingerprint = HexBytes::new([0; 32]);
        } else {
            owner.device = other_id();
        }
        let pin = Arc::new(FixedPins {
            state: PinState::Confirmed(
                ConfirmedPin::new(owner, keys.clone(), keys.fingerprint()).unwrap(),
            ),
            unavailable: false,
        });
        let server = ScriptedServer::start(vec![
            Reply::json(200, &status_json(Some(&signed))),
            Reply::json(200, &status_json(Some(&signed))),
        ])
        .await
        .unwrap();
        assert!(matches!(
            ordinary_client(&server, true)
                .submit(logical())
                .await
                .unwrap(),
            SendOutcome::RecipientStored(_)
        ));
        let c = client(
            &server,
            true,
            keyed_source(vec![record]),
            pin,
            SpyFacility::new(true),
        );
        assert!(
            matches!(c.submit(logical()).await.unwrap(), SendOutcome::Pending(PendingDetail::ReceiptUnverified { reason }) if reason == "FingerprintMismatch"),
            "a valid signature must not bypass the pin identity/fingerprint comparison"
        );
        assert_eq!(server.requests().await.len(), 2);
    }
}

#[tokio::test]
async fn receipt_pin_unavailable_and_source_mismatch_are_reported() {
    let signed = receipt();
    for source_mismatch in [false, true] {
        let mut record = persisted();
        if source_mismatch {
            record.namespace = "other".into();
        }
        let pin = FixedPins {
            state: pins(false).state.clone(),
            unavailable: !source_mismatch,
        };
        let server = ScriptedServer::start(vec![
            Reply::json(200, &status_json(Some(&signed))),
            Reply::json(200, &status_json(Some(&signed))),
        ])
        .await
        .unwrap();
        assert!(matches!(
            ordinary_client(&server, true)
                .status(logical())
                .await
                .unwrap()
                .receipt,
            Some(ReceiptVerification::Verified(_))
        ));
        let c = client(
            &server,
            true,
            keyed_source(vec![record]),
            Arc::new(pin),
            SpyFacility::new(true),
        );
        let result = c.status(logical()).await.unwrap();
        let expected = if source_mismatch {
            ReceiptRejection::SourceMismatch
        } else {
            ReceiptRejection::PinUnavailable
        };
        assert!(
            matches!(result.receipt, Some(ReceiptVerification::Rejected(reason)) if reason == expected),
            "the source/pin failure must be reported distinctly"
        );
        assert_eq!(server.requests().await.len(), 2);
    }
}
