//! Real HTTP adapter witnesses for ADR-105's sender receipt amendment.
use super::*;
use crate::adapter::{receipt_cursor_source, NodeChannel, NODE_RECEIPT_CURSOR_GENERATION};
use chrono::{DateTime, Utc};
use khive_channel::{
    Channel, ChannelCheckpoint, ChannelEnvelope, ReceiptReadFailure, ReceiptRejectionReason,
    SenderReceiptClaim, SenderReceiptResult, StoredChannelCheckpoint,
    LOGICAL_MESSAGE_ID_METADATA_KEY,
};

fn since() -> DateTime<Utc> {
    "2026-09-24T00:00:00Z".parse().unwrap()
}
fn envelope() -> ChannelEnvelope {
    let mut envelope = ChannelEnvelope::new("sender", "recipient", "persisted body");
    envelope.metadata.insert(
        LOGICAL_MESSAGE_ID_METADATA_KEY.into(),
        logical().to_string(),
    );
    envelope
}
fn checkpoint(high_water: Option<u64>) -> StoredChannelCheckpoint {
    StoredChannelCheckpoint {
        checkpoint: ChannelCheckpoint {
            source: receipt_cursor_source(&binding(true)),
            generation: NODE_RECEIPT_CURSOR_GENERATION,
            high_water,
        },
        committed_at: since(),
    }
}
fn adapter(server: &ScriptedServer, sender: bool) -> NodeChannel {
    NodeChannel::new(ordinary_client(server, sender), PollWait::new(0).unwrap())
}
struct RecordingPins {
    retained: Vec<ConfirmedPin>,
    requested: std::sync::Mutex<Vec<PinIdentity>>,
}
#[async_trait]
impl PinSource for RecordingPins {
    async fn resolve(&self, identity: &PinIdentity) -> Result<PinState, PinSourceError> {
        self.requested.lock().unwrap().push(identity.clone());
        Ok(self
            .retained
            .iter()
            .find(|pin| pin.identity() == identity)
            .cloned()
            .map(PinState::Confirmed)
            .unwrap_or(PinState::UnconfirmedEpoch { candidate: None }))
    }
}
fn retained_pins() -> Arc<RecordingPins> {
    let old = facility(false).public_keys();
    let new = facility(true).public_keys();
    let mut next = identity(false);
    next.epoch = Epoch::new(next.epoch.get() + 1).unwrap();
    Arc::new(RecordingPins {
        retained: vec![
            ConfirmedPin::new(identity(false), old.clone(), old.fingerprint()).unwrap(),
            ConfirmedPin::new(next, new.clone(), new.fingerprint()).unwrap(),
        ],
        requested: std::sync::Mutex::new(Vec::new()),
    })
}
struct OrderedPins(std::sync::Mutex<std::collections::VecDeque<Result<PinState, PinSourceError>>>);
#[async_trait]
impl PinSource for OrderedPins {
    async fn resolve(&self, _: &PinIdentity) -> Result<PinState, PinSourceError> {
        self.0
            .lock()
            .unwrap()
            .pop_front()
            .expect("one pin resolution per eligible receipt")
    }
}

#[tokio::test]
async fn adapter_rejects_service_key_and_unverifiable_signature_on_all_paths() {
    let keys_b = facility(true).public_keys();
    let observed = contact_response(keys_b.clone());
    let forged = WireReceipt::sign(
        receipt().binding,
        ReceiptDisposition::Stored,
        &facility(true),
    )
    .unwrap();
    let mut invalid = receipt();
    invalid.signature = Base64Bytes::new([0; 64]);
    for bad in [forged, invalid] {
        let server = ScriptedServer::start(vec![
            Reply::json(200, &serde_json::to_string(&observed).unwrap()),
            Reply::json(200, &poll_body(None, vec![bad.clone()])),
            Reply::json(200, &status_json(Some(&bad))),
            Reply::json(200, &status_json(Some(&bad))),
        ])
        .await
        .unwrap();
        let pin = retained_pins();
        let c = client(
            &server,
            true,
            Arc::new(FixedSource(Ok(Some(persisted())))),
            pin.clone(),
            SpyFacility::new(true),
        );
        let contact = c.contact(binding(false).agent.into_uuid()).await.unwrap();
        assert_eq!(contact.observed.signing_public_key, keys_b.signing);
        assert!(
            !contact.matches_owner_pin,
            "offered B never confirms pinned A"
        );
        assert!(
            receipt()
                .verify(&facility(false).public_keys().signing)
                .is_ok(),
            "valid signed setup uses retained A"
        );
        let channel = NodeChannel::new(c.clone(), PollWait::new(0).unwrap());
        let page = channel.poll_deliveries(since(), None).await.unwrap();
        assert!(
            matches!(
                &page.receipt_results()[0],
                SenderReceiptResult::Rejected {
                    reason: ReceiptRejectionReason::InvalidSignature,
                    ..
                }
            ),
            "poll refuses service B and signatures valid under no key"
        );
        assert!(
            matches!(channel.send_with_receipt(envelope()).await.unwrap(), SendOutcome::Pending(PendingDetail::ReceiptUnverified { reason }) if reason == "InvalidSignature")
        );
        assert!(matches!(
            c.status(logical()).await.unwrap().receipt,
            Some(ReceiptVerification::Rejected(
                ReceiptRejection::InvalidSignature
            ))
        ));
        assert_eq!(
            *pin.requested.lock().unwrap(),
            vec![identity(false); 4],
            "all paths resolve the row's epoch N"
        );
        assert_eq!(server.requests().await.len(), 4);
    }
}

#[tokio::test]
async fn adapter_three_result_kinds_preserve_original_page_order_and_claims() {
    let valid = receipt();
    let mut mismatch = valid.clone();
    mismatch.binding.contact_generation = Epoch::new(4).unwrap();
    let malformed = r#"{"seq":1,"receipt":false,"recorded_at":"2026-09-24T00:00:00Z"}"#;
    let items: Vec<String> = [mismatch, valid.clone(), valid.clone(), valid.clone()]
        .iter()
        .enumerate()
        .map(|(i, r)| receipt_item_json(r, i as u64 + 2))
        .collect();
    let mut slices = vec![malformed];
    slices.extend(items.iter().map(String::as_str));
    let server = ScriptedServer::start(vec![Reply::json(
        200,
        &poll_body_slices(&[], &slices, 5, "2026-09-24T00:00:00Z"),
    )])
    .await
    .unwrap();
    let source = Arc::new(OrderedSource {
        outcomes: std::sync::Mutex::new(
            vec![
                Ok(Some(persisted())),
                Ok(Some(persisted())),
                Ok(None),
                Ok(Some(persisted())),
            ]
            .into(),
        ),
        requested: std::sync::Mutex::new(Vec::new()),
    });
    let pin = Arc::new(OrderedPins(std::sync::Mutex::new(
        vec![
            Ok(pins(false).state.clone()),
            Err(PinSourceError("local read".into())),
        ]
        .into(),
    )));
    let channel = NodeChannel::new(
        client(
            &server,
            true,
            source.clone(),
            pin.clone(),
            SpyFacility::new(true),
        ),
        PollWait::new(0).unwrap(),
    );
    let page = channel.poll_deliveries(since(), None).await.unwrap();
    let results = page.receipt_results();
    assert_eq!(results.len(), 5);
    assert!(
        matches!(&results[0], SenderReceiptResult::Rejected { claim, reason: ReceiptRejectionReason::ParseFailure } if claim == &SenderReceiptClaim::default())
    );
    let claim = SenderReceiptClaim {
        logical_message_id: Some(logical()),
        recipient_device_id: Some(binding(false).device.into_uuid()),
        recipient_key_epoch: Some(2),
    };
    assert!(
        matches!(&results[1], SenderReceiptResult::Rejected { claim: actual, reason: ReceiptRejectionReason::BindingMismatch } if actual == &claim)
    );
    assert!(
        matches!(&results[2], SenderReceiptResult::Verified(v) if v.receipt() == &valid.to_channel())
    );
    assert!(
        matches!(&results[3], SenderReceiptResult::Rejected { claim: actual, reason: ReceiptRejectionReason::SourceMissing } if actual == &claim)
    );
    assert!(
        matches!(&results[4], SenderReceiptResult::Unhandled { claim: actual, reason: ReceiptReadFailure::PinUnavailable } if actual == &claim)
    );
    assert_eq!(page.receipts_cursor(), Some(5));
    assert!(page.page().next_checkpoint.is_none());
    assert!(source.outcomes.lock().unwrap().is_empty());
    assert!(pin.0.lock().unwrap().is_empty());
    assert_eq!(*source.requested.lock().unwrap(), vec![logical(); 4]);
}

#[tokio::test]
async fn adapter_local_source_failure_is_unhandled_on_poll_and_status() {
    let server = ScriptedServer::start(vec![
        Reply::json(200, &poll_body(None, vec![receipt()])),
        Reply::json(200, &status_json(Some(&receipt()))),
    ])
    .await
    .unwrap();
    let c = client(
        &server,
        true,
        Arc::new(FixedSource(Err(SourceError("local read".into())))),
        pins(false),
        SpyFacility::new(true),
    );
    let channel = NodeChannel::new(c.clone(), PollWait::new(0).unwrap());
    let page = channel.poll_deliveries(since(), None).await.unwrap();
    assert!(matches!(
        &page.receipt_results()[0],
        SenderReceiptResult::Unhandled {
            reason: ReceiptReadFailure::SourceUnavailable,
            ..
        }
    ));
    assert!(matches!(
        c.status(logical()).await.unwrap().receipt,
        Some(ReceiptVerification::Unhandled(
            ReceiptReadFailure::SourceUnavailable
        ))
    ));
}

#[tokio::test]
async fn adapter_local_pin_failure_is_unhandled_on_status_and_transport_on_submit() {
    let server = ScriptedServer::start(vec![
        Reply::json(200, &status_json(Some(&receipt()))),
        Reply::json(200, &status_json(Some(&receipt()))),
    ])
    .await
    .unwrap();
    let pin = Arc::new(FixedPins {
        state: PinState::NonContact,
        unavailable: true,
    });
    let c = client(
        &server,
        true,
        Arc::new(FixedSource(Ok(Some(persisted())))),
        pin,
        SpyFacility::new(true),
    );
    assert!(matches!(
        c.status(logical()).await.unwrap().receipt,
        Some(ReceiptVerification::Unhandled(
            ReceiptReadFailure::PinUnavailable
        ))
    ));
    let channel = NodeChannel::new(c, PollWait::new(0).unwrap());
    assert!(matches!(
        channel.send_with_receipt(envelope()).await,
        Err(ChannelError::Transport(_))
    ));
}

#[tokio::test]
async fn adapter_pages_carry_wire_cursor_without_envelopes_tickets_or_checkpoint() {
    let delivery_json = serde_json::to_string(&delivery()).unwrap();
    let receipt_json = receipt_item_json(&receipt(), 8);
    for (deliveries, receipts, cursor) in [
        (vec![], vec![], 0),
        (vec![], vec![receipt_json.as_str()], 8),
        (vec![delivery_json.as_str()], vec![], 7),
    ] {
        for stored in [None, Some(checkpoint(Some(7)))] {
            let wire_cursor = if stored.is_some() {
                cursor.max(7)
            } else {
                cursor
            };
            let server = ScriptedServer::start(vec![Reply::json(
                200,
                &poll_body_slices(&deliveries, &receipts, wire_cursor, "2026-09-24T00:00:00Z"),
            )])
            .await
            .unwrap();
            let channel = adapter(&server, true);
            let page = channel
                .poll_deliveries(since(), stored.as_ref())
                .await
                .unwrap();
            assert!(page.page().next_checkpoint.is_none());
            assert!(page.page().envelopes.is_empty());
            assert!(page.tickets().is_empty());
            assert_eq!(page.receipts_cursor(), Some(wire_cursor));
            assert_eq!(page.receipt_results().len(), receipts.len());
            let after = stored
                .as_ref()
                .and_then(|s| s.checkpoint.high_water)
                .unwrap_or(0);
            assert_eq!(
                server.requests().await[0].target,
                format!("/node/v1/poll?wait=0&receipts_after={after}")
            );
        }
    }
}

#[tokio::test]
async fn adapter_cursor_identity_generation_and_regression_are_checked() {
    let server = ScriptedServer::start(vec![
        Reply::json(200, &poll_body(None, vec![])),
        Reply::json(200, &poll_body(None, vec![])),
    ])
    .await
    .unwrap();
    let channel = adapter(&server, true);
    let mut foreign = checkpoint(Some(91));
    foreign.checkpoint.source = receipt_cursor_source(&binding(false));
    assert_eq!(
        channel
            .poll_deliveries(since(), Some(&foreign))
            .await
            .unwrap()
            .receipts_cursor(),
        Some(0)
    );
    assert!(server.requests().await[0]
        .target
        .ends_with("receipts_after=0"));
    let mut wrong = checkpoint(Some(9));
    wrong.checkpoint.generation += 1;
    assert!(matches!(
        channel.poll_deliveries(since(), Some(&wrong)).await,
        Err(ChannelError::Config(_))
    ));
    assert_eq!(
        server.requests().await.len(),
        1,
        "generation failure precedes HTTP"
    );
    let error = channel
        .poll_deliveries(since(), Some(&checkpoint(Some(9))))
        .await
        .unwrap_err();
    assert!(
        matches!(error, ChannelError::Transport(ref message) if message.contains("cursor 0") && message.contains("receipts_after 9")),
        "regressed page is refused whole"
    );
    assert_eq!(server.requests().await.len(), 2);
}

#[tokio::test]
async fn adapter_unset_high_water_and_poll_wait_keep_protocol_bounds() {
    let server = ScriptedServer::start(vec![Reply::json(200, &poll_body(None, vec![]))])
        .await
        .unwrap();
    let channel = NodeChannel::new(ordinary_client(&server, true), PollWait::new(25).unwrap());
    let page = channel
        .poll_deliveries(since(), Some(&checkpoint(None)))
        .await
        .unwrap();
    assert_eq!(page.receipts_cursor(), Some(0));
    assert_eq!(
        server.requests().await[0].target,
        "/node/v1/poll?wait=25&receipts_after=0"
    );
    assert!(matches!(
        channel
            .poll_deliveries(since(), Some(&checkpoint(Some(u64::MAX))))
            .await,
        Err(ChannelError::Config(_))
    ));
    assert_eq!(
        server.requests().await.len(),
        1,
        "out-of-protocol cursor never reaches HTTP"
    );
    assert!(PollWait::new(26).is_err());
}

#[tokio::test]
async fn adapter_retains_epoch_n_after_confirming_n_plus_one() {
    let pin = retained_pins();
    let next = pin.retained[1].identity().clone();
    assert!(
        matches!(pin.resolve(&next).await.unwrap(), PinState::Confirmed(p) if p.identity() == &next),
        "N+1 is actually confirmed first"
    );
    pin.requested.lock().unwrap().clear();
    let server = ScriptedServer::start(vec![Reply::json(200, &poll_body(None, vec![receipt()]))])
        .await
        .unwrap();
    let channel = NodeChannel::new(
        client(
            &server,
            true,
            Arc::new(FixedSource(Ok(Some(persisted())))),
            pin.clone(),
            SpyFacility::new(true),
        ),
        PollWait::new(0).unwrap(),
    );
    let page = channel.poll_deliveries(since(), None).await.unwrap();
    assert!(
        matches!(&page.receipt_results()[0], SenderReceiptResult::Verified(v) if v.receipt() == &receipt().to_channel()),
        "retained N verifies after N+1 confirmation"
    );
    assert_eq!(*pin.requested.lock().unwrap(), vec![identity(false)]);
    assert!(page.page().next_checkpoint.is_none());
    assert_eq!(page.receipts_cursor(), Some(1));
}

#[tokio::test]
async fn adapter_submission_a9_outcomes_and_sealed_receipts_are_exact() {
    for (status, body, expected) in [
        (202, admission(), "admitted"),
        (200, admission(), "admitted"),
        (402, "{\"error\":\"insufficient_credit\"}".into(), "credit"),
        (
            409,
            "{\"error\":\"recipient_key_changed\"}".into(),
            "changed",
        ),
        (401, "{\"error\":\"unauthenticated\"}".into(), "auth"),
        (403, "{\"error\":\"invalid_request\"}".into(), "permanent"),
    ] {
        let server = ScriptedServer::start(vec![Reply::json(status, &body)])
            .await
            .unwrap();
        let channel = adapter(&server, true);
        assert_eq!(channel.kind(), "khive");
        assert_eq!(channel.slug(), binding(true).slug);
        let result = channel.send_with_receipt(envelope()).await;
        match expected {
            "admitted" => assert!(matches!(
                result,
                Ok(SendOutcome::Pending(PendingDetail::Admitted { .. }))
            )),
            "credit" => assert!(matches!(
                result,
                Ok(SendOutcome::Pending(PendingDetail::Held(
                    HoldReason::InsufficientCredit
                )))
            )),
            "changed" => assert!(matches!(
                result,
                Ok(SendOutcome::Pending(PendingDetail::Held(
                    HoldReason::RecipientKeyChanged
                )))
            )),
            "auth" => assert!(matches!(result, Err(ChannelError::Auth(_)))),
            _ => assert!(matches!(result, Err(ChannelError::PermanentTransport(_)))),
        }
    }
    for disposition in [ReceiptDisposition::Stored, ReceiptDisposition::Quarantined] {
        let r = WireReceipt::sign(receipt().binding, disposition, &facility(false)).unwrap();
        let mut status: Value = serde_json::from_str(&status_json(Some(&r))).unwrap();
        status["state"] = json!(if disposition == ReceiptDisposition::Stored {
            "recipient_stored"
        } else {
            "recipient_quarantined"
        });
        let server = ScriptedServer::start(vec![Reply::json(200, &status.to_string())])
            .await
            .unwrap();
        match adapter(&server, true)
            .send_with_receipt(envelope())
            .await
            .unwrap()
        {
            SendOutcome::RecipientStored(v) if disposition == ReceiptDisposition::Stored => {
                assert_eq!(v.receipt(), &r.to_channel())
            }
            SendOutcome::RecipientQuarantined(v)
                if disposition == ReceiptDisposition::Quarantined =>
            {
                assert_eq!(v.receipt(), &r.to_channel())
            }
            other => panic!("expected sealed disposition, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn adapter_ack_a9_outcomes_are_exact() {
    for (status, body, expected) in [
        (200, "{\"recorded\":true}", "ok"),
        (404, "{\"error\":\"not_found\"}", "ok"),
        (409, "{\"error\":\"receipt_conflict\"}", "permanent"),
        (422, "{\"error\":\"receipt_invalid\"}", "permanent"),
        (503, "{\"error\":\"capacity_exhausted\"}", "transport"),
        (401, "{\"error\":\"unauthenticated\"}", "auth"),
    ] {
        let server = ScriptedServer::start(vec![Reply::json(status, body)])
            .await
            .unwrap();
        let result = adapter(&server, false)
            .acknowledge_receipt(&receipt().to_channel())
            .await;
        match expected {
            "ok" => assert!(result.is_ok(), "HTTP {status} {body} is done"),
            "permanent" => assert!(matches!(result, Err(ChannelError::PermanentTransport(_)))),
            "transport" => assert!(matches!(result, Err(ChannelError::Transport(_)))),
            _ => assert!(matches!(result, Err(ChannelError::Auth(_)))),
        }
    }
}

#[tokio::test]
async fn adapter_replay_uses_only_the_supplied_checkpoint() {
    let wire = poll_body_slices(
        &[],
        &[&receipt_item_json(&receipt(), 8)],
        8,
        "2026-09-24T00:00:00Z",
    );
    let server = ScriptedServer::start(vec![Reply::json(200, &wire), Reply::json(200, &wire)])
        .await
        .unwrap();
    let channel = adapter(&server, true);
    let stored = checkpoint(Some(7));
    let first = channel
        .poll_deliveries(since(), Some(&stored))
        .await
        .unwrap();
    let second = channel
        .poll_deliveries(since(), Some(&stored))
        .await
        .unwrap();
    for page in [&first, &second] {
        assert!(page.page().next_checkpoint.is_none());
        assert_eq!(page.receipts_cursor(), Some(8));
        assert!(
            matches!(&page.receipt_results()[0], SenderReceiptResult::Verified(v) if v.receipt() == &receipt().to_channel())
        );
    }
    assert_eq!(
        format!("{:?}", first.receipt_results()),
        format!("{:?}", second.receipt_results())
    );
    let requests = server.requests().await;
    assert_eq!(requests[0].target, "/node/v1/poll?wait=0&receipts_after=7");
    assert_eq!(
        requests[1].target, requests[0].target,
        "retry retains the same durable input"
    );
}

#[tokio::test]
async fn adapter_refuses_legacy_methods_and_invalid_message_metadata_before_http() {
    let server = ScriptedServer::start(vec![]).await.unwrap();
    let channel = adapter(&server, true);
    assert!(matches!(
        channel.send(envelope()).await,
        Err(ChannelError::Config(_))
    ));
    assert!(matches!(
        channel.poll(since()).await,
        Err(ChannelError::Config(_))
    ));
    for raw in [
        None,
        Some(""),
        Some("0192000000007000800000000000a003"),
        Some("01920000-0000-7000-8000-00000000A003"),
    ] {
        let mut value = envelope();
        value.metadata.clear();
        if let Some(raw) = raw {
            value
                .metadata
                .insert(LOGICAL_MESSAGE_ID_METADATA_KEY.into(), raw.into());
        }
        assert!(matches!(
            channel.send_with_receipt(value).await,
            Err(ChannelError::InvalidEnvelope(_))
        ));
    }
    assert!(server.requests().await.is_empty());
    let source = Arc::new(FixedSource(Err(SourceError("local storage".into()))));
    let channel = NodeChannel::new(
        client(&server, true, source, pins(false), SpyFacility::new(true)),
        PollWait::new(0).unwrap(),
    );
    let mut noncanonical = envelope();
    noncanonical.metadata.insert(
        LOGICAL_MESSAGE_ID_METADATA_KEY.into(),
        logical().to_string().to_ascii_uppercase(),
    );
    assert!(matches!(
        channel.send_with_receipt(noncanonical).await,
        Err(ChannelError::InvalidEnvelope(_))
    ));
    assert!(matches!(
        channel.send_with_receipt(envelope()).await,
        Err(ChannelError::Transport(_))
    ));
    assert!(server.requests().await.is_empty());
}
