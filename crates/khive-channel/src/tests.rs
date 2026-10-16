use super::*;
use std::sync::Mutex;

struct LegacyChannel {
    sent: Mutex<Vec<ChannelEnvelope>>,
    inbound: Vec<ChannelEnvelope>,
    fail: bool,
}

#[async_trait]
impl Channel for LegacyChannel {
    fn kind(&self) -> &'static str {
        "legacy"
    }

    async fn send(&self, envelope: ChannelEnvelope) -> Result<(), ChannelError> {
        if self.fail {
            return Err(ChannelError::Transport("send unavailable".into()));
        }
        self.sent.lock().unwrap().push(envelope);
        Ok(())
    }

    async fn poll(&self, _since: DateTime<Utc>) -> Result<Vec<ChannelEnvelope>, ChannelError> {
        if self.fail {
            return Err(ChannelError::UnauthorizedSender("poll denied".into()));
        }
        Ok(self.inbound.clone())
    }
}

fn receipt(disposition: ReceiptDisposition) -> DeliveryReceipt {
    DeliveryReceipt {
        binding: DeliveryReceiptBinding {
            protocol_version: 1,
            logical_message_id: Uuid::new_v4(),
            sender_agent_id: "sender-agent".into(),
            recipient_agent_id: "recipient-agent".into(),
            recipient_device_id: Uuid::new_v4(),
            recipient_key_epoch: 2,
            contact_generation: 3,
            delivery_attempt_id: Uuid::new_v4(),
        },
        disposition,
        signature: vec![1, 2, 3],
    }
}

fn verified_receipt(disposition: ReceiptDisposition) -> VerifiedRecipientReceipt {
    use ring::signature::{Ed25519KeyPair, KeyPair};
    let mut receipt = receipt(disposition);
    receipt.binding.sender_agent_id = Uuid::nil().to_string();
    receipt.binding.recipient_agent_id = Uuid::nil().to_string();
    let key = Ed25519KeyPair::from_seed_unchecked(&[17; 32]).unwrap();
    receipt.signature = key
        .sign(&receipt_signing_input(&receipt).unwrap())
        .as_ref()
        .to_vec();
    let pinned: [u8; 32] = key.public_key().as_ref().try_into().unwrap();
    VerifiedRecipientReceipt::verify(receipt, &pinned).unwrap()
}

#[tokio::test]
async fn receipt_defaults_preserve_legacy_send_and_poll() {
    let envelope = ChannelEnvelope::new("legacy:sender", "legacy:recipient", "body")
        .with_subject("subject")
        .with_sent_at(Utc::now())
        .with_external_id("external-id")
        .with_correlation("thread-id")
        .with_quarantine_replay(vec![0, 255, 13, 10], "legacy:sender");
    let expected_bytes = serde_json::to_vec(&envelope).unwrap();
    let adapter = LegacyChannel {
        sent: Mutex::new(Vec::new()),
        inbound: vec![envelope.clone()],
        fail: false,
    };
    let channel: &dyn Channel = &adapter;
    assert_eq!(
        channel.send_with_receipt(envelope).await.unwrap(),
        SendOutcome::LegacyAccepted
    );
    {
        let sent = adapter.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(serde_json::to_vec(&sent[0]).unwrap(), expected_bytes);
        assert_eq!(
            sent[0].quarantine_replay.as_ref().unwrap().bytes,
            [0, 255, 13, 10]
        );
    }
    let deliveries = channel.poll_deliveries(Utc::now(), None).await.unwrap();
    assert!(deliveries.receipt_results().is_empty());
    assert_eq!(deliveries.receipts_cursor(), None);
    assert_eq!(deliveries.tickets().len(), 1);
    assert!(deliveries.tickets().iter().all(Option::is_none));
    let (page, tickets) = deliveries.into_parts();
    assert_eq!(page.envelopes.len(), tickets.len());
    assert_eq!(
        serde_json::to_vec(&page.envelopes[0]).unwrap(),
        expected_bytes
    );
    assert_eq!(
        page.envelopes[0].quarantine_replay.as_ref().unwrap().bytes,
        [0, 255, 13, 10]
    );
    assert!(page.next_checkpoint.is_none());

    let error = channel
        .acknowledge_receipt(&receipt(ReceiptDisposition::Stored))
        .await
        .unwrap_err();
    assert_eq!(
        error.delivery_failure_class(),
        DeliveryFailureClass::Permanent
    );
    assert!(
        matches!(error, ChannelError::Config(message) if message == "receipts unsupported by legacy")
    );
}

#[tokio::test]
async fn receipt_defaults_propagate_legacy_errors() {
    let channel = LegacyChannel {
        sent: Mutex::new(Vec::new()),
        inbound: vec![],
        fail: true,
    };
    assert!(matches!(
        channel.send_with_receipt(ChannelEnvelope::new("a", "b", "c")).await,
        Err(ChannelError::Transport(message)) if message == "send unavailable"
    ));
    assert!(matches!(
        channel.poll_deliveries(Utc::now(), None).await,
        Err(ChannelError::UnauthorizedSender(message)) if message == "poll denied"
    ));
    assert!(channel.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn poll_deliveries_preserves_overridden_page_and_checkpoint() {
    struct CheckpointChannel {
        since: DateTime<Utc>,
        checkpoint: StoredChannelCheckpoint,
    }

    #[async_trait]
    impl Channel for CheckpointChannel {
        fn kind(&self) -> &'static str {
            "checkpoint"
        }

        async fn send(&self, _: ChannelEnvelope) -> Result<(), ChannelError> {
            panic!("poll_deliveries must not send");
        }

        async fn poll(&self, _: DateTime<Utc>) -> Result<Vec<ChannelEnvelope>, ChannelError> {
            panic!("poll_deliveries must use the poll_page override");
        }

        async fn poll_page(
            &self,
            since: DateTime<Utc>,
            checkpoint: Option<&StoredChannelCheckpoint>,
        ) -> Result<ChannelPollPage, ChannelError> {
            assert_eq!(since, self.since);
            assert_eq!(checkpoint, Some(&self.checkpoint));
            Ok(ChannelPollPage {
                envelopes: vec![ChannelEnvelope::new("a", "b", "checkpointed")],
                next_checkpoint: Some(ChannelCheckpoint {
                    high_water: Some(43),
                    ..self.checkpoint.checkpoint.clone()
                }),
            })
        }
    }

    let channel = CheckpointChannel {
        since: Utc::now(),
        checkpoint: StoredChannelCheckpoint {
            checkpoint: ChannelCheckpoint {
                source: "source".into(),
                generation: 7,
                high_water: Some(42),
            },
            committed_at: Utc::now(),
        },
    };
    let page = channel
        .poll_deliveries(channel.since, Some(&channel.checkpoint))
        .await
        .unwrap();
    assert_eq!(page.page().envelopes[0].content, "checkpointed");
    assert_eq!(
        page.page().next_checkpoint,
        Some(ChannelCheckpoint {
            high_water: Some(43),
            ..channel.checkpoint.checkpoint.clone()
        })
    );
    assert!(page.tickets()[0].is_none());
    assert!(page.receipt_results().is_empty());
    assert_eq!(page.receipts_cursor(), None);
}

#[test]
fn delivery_page_checks_counts_and_retains_ticket_order() {
    let make_page = || {
        ChannelPollPage::stateless(vec![
            ChannelEnvelope::new("a", "b", "first"),
            ChannelEnvelope::new("a", "b", "second"),
        ])
    };
    for count in [0, 1, 3] {
        let tickets = (0..count).map(|_| None).collect();
        let error = DeliveryPage::new(make_page(), tickets).unwrap_err();
        assert!(matches!(error, ChannelError::InvalidEnvelope(_)));
        assert_eq!(
            error.delivery_failure_class(),
            DeliveryFailureClass::Permanent
        );
    }
    let binding = receipt(ReceiptDisposition::Stored).binding;
    let ticket = InboundReceiptTicket::new(binding.clone(), 9);
    let page = DeliveryPage::new(make_page(), vec![None, Some(ticket)]).unwrap();
    assert_eq!(page.page().envelopes.len(), 2);
    assert!(page.tickets()[0].is_none());
    let ticket = page.tickets()[1].as_ref().unwrap();
    assert_eq!(ticket.binding(), &binding);
    assert_eq!(ticket.sender_key_epoch(), 9);
    assert!(DeliveryPage::new(ChannelPollPage::stateless(vec![]), vec![]).is_ok());
}

#[test]
fn receipt_disposition_is_closed_and_outcomes_must_agree() {
    let stored = receipt(ReceiptDisposition::Stored);
    assert!(SendOutcome::LegacyAccepted.validate_receipt().is_ok());
    assert!(SendOutcome::Pending(PendingDetail::Admitted {
        admitted_at: Utc::now(),
    })
    .validate_receipt()
    .is_ok());
    assert!(
        SendOutcome::Pending(PendingDetail::Held(HoldReason::InsufficientCredit))
            .validate_receipt()
            .is_ok()
    );
    assert!(
        SendOutcome::Pending(PendingDetail::Held(HoldReason::RecipientKeyChanged))
            .validate_receipt()
            .is_ok()
    );
    assert!(SendOutcome::Pending(PendingDetail::ReceiptUnverified {
        reason: "receipt signature does not match the pinned key".into(),
    })
    .validate_receipt()
    .is_ok());
    let verified_stored = verified_receipt(ReceiptDisposition::Stored);
    let verified_quarantined = verified_receipt(ReceiptDisposition::Quarantined);
    assert!(SendOutcome::RecipientStored(verified_stored.clone())
        .validate_receipt()
        .is_ok());
    assert!(
        SendOutcome::RecipientQuarantined(verified_quarantined.clone())
            .validate_receipt()
            .is_ok()
    );
    assert!(SendOutcome::RecipientStored(verified_quarantined)
        .validate_receipt()
        .is_err());
    assert!(SendOutcome::RecipientQuarantined(verified_stored)
        .validate_receipt()
        .is_err());
    let mut value = serde_json::to_value(&stored).unwrap();
    assert_eq!(value["disposition"], "stored");
    assert_eq!(
        serde_json::from_value::<DeliveryReceipt>(value.clone()).unwrap(),
        stored
    );
    value["disposition"] = serde_json::json!("pending");
    assert!(serde_json::from_value::<DeliveryReceipt>(value).is_err());
}

struct MockChannel {
    sent: Arc<Mutex<Vec<ChannelEnvelope>>>,
    inbound: Vec<ChannelEnvelope>,
    // `None` -> `slug()` falls back to the trait default (`kind()`).
    // `Some(_)` -> distinct per-credential identity, for #606's
    // same-kind-different-slug regressions.
    slug: Option<String>,
}

impl MockChannel {
    fn new(inbound: Vec<ChannelEnvelope>) -> Self {
        Self {
            sent: Arc::new(Mutex::new(Vec::new())),
            inbound,
            slug: None,
        }
    }

    fn with_slug(inbound: Vec<ChannelEnvelope>, slug: impl Into<String>) -> Self {
        Self {
            sent: Arc::new(Mutex::new(Vec::new())),
            inbound,
            slug: Some(slug.into()),
        }
    }
}

#[async_trait]
impl Channel for MockChannel {
    fn kind(&self) -> &'static str {
        "mock"
    }

    fn slug(&self) -> String {
        self.slug.clone().unwrap_or_else(|| self.kind().to_string())
    }

    async fn send(&self, envelope: ChannelEnvelope) -> Result<(), ChannelError> {
        self.sent.lock().unwrap().push(envelope);
        Ok(())
    }

    async fn poll(&self, _since: DateTime<Utc>) -> Result<Vec<ChannelEnvelope>, ChannelError> {
        Ok(self.inbound.clone())
    }
}

#[test]
fn envelope_builder_fields() {
    let ts = Utc::now();
    let env = ChannelEnvelope::new("email:a@example.com", "email:b@example.com", "hello")
        .with_subject("Test")
        .with_sent_at(ts)
        .with_external_id("<msg1@example.com>")
        .with_correlation("correlation-uuid")
        .with_message_id("<abc123@example.com>")
        .with_wire_message_id("<wire123@example.com>")
        .with_wire_references("<ref1@example.com> <ref2@example.com>")
        .with_in_reply_to("<parent123@example.com>")
        .with_references("<ref1@example.com> <parent123@example.com>");

    assert_eq!(env.from, "email:a@example.com");
    assert_eq!(env.to, "email:b@example.com");
    assert_eq!(env.content, "hello");
    assert_eq!(env.subject.as_deref(), Some("Test"));
    assert_eq!(env.sent_at, Some(ts));
    assert_eq!(env.external_id.as_deref(), Some("<msg1@example.com>"));
    assert_eq!(
        env.correlation_external_id.as_deref(),
        Some("correlation-uuid")
    );
    assert_eq!(env.message_id.as_deref(), Some("<abc123@example.com>"));
    assert_eq!(
        env.wire_message_id.as_deref(),
        Some("<wire123@example.com>")
    );
    assert_eq!(
        env.wire_references.as_deref(),
        Some("<ref1@example.com> <ref2@example.com>")
    );
    assert_eq!(env.in_reply_to.as_deref(), Some("<parent123@example.com>"));
    assert_eq!(
        env.references.as_deref(),
        Some("<ref1@example.com> <parent123@example.com>")
    );
}

#[test]
fn envelope_new_defaults_wire_message_id_and_in_reply_to_to_none() {
    let env = ChannelEnvelope::new("email:a@example.com", "email:b@example.com", "hello");
    assert_eq!(env.wire_message_id, None);
    assert_eq!(env.wire_references, None);
    assert_eq!(env.in_reply_to, None);
    assert_eq!(env.references, None);
}

#[test]
fn registry_register_and_get() {
    let mut reg = ChannelRegistry::new();
    let ch = Arc::new(MockChannel::new(vec![]));
    reg.register(ch);
    assert!(reg.get("mock").is_some());
    assert!(reg.get("email").is_none());
    assert_eq!(reg.len(), 1);
    assert!(!reg.is_empty());
}

#[test]
fn registry_replaces_existing_same_composite_identity() {
    // Two registrations with the SAME (kind, slug) — both default to
    // slug() == kind() here — still replace in place, matching the
    // pre-#606 single-credential-per-kind semantics.
    let mut reg = ChannelRegistry::new();
    reg.register(Arc::new(MockChannel::new(vec![])));
    reg.register(Arc::new(MockChannel::new(vec![])));
    assert_eq!(reg.len(), 1, "same (kind, slug) replaces");
}

#[test]
fn registry_does_not_collapse_same_kind_distinct_slug() {
    // #606: two accounts sharing
    // `kind()` but with distinct `slug()` values (e.g. two mailboxes)
    // must coexist as two registered adapters, not collapse into one.
    let mut reg = ChannelRegistry::new();
    reg.register(Arc::new(MockChannel::with_slug(vec![], "a@example.com")));
    reg.register(Arc::new(MockChannel::with_slug(vec![], "b@example.com")));
    assert_eq!(
        reg.len(),
        2,
        "same kind + distinct slug must coexist, not collapse"
    );
    assert!(reg.get_by_slug("mock", "a@example.com").is_some());
    assert!(reg.get_by_slug("mock", "b@example.com").is_some());
}

#[test]
fn registry_iter_yields_all() {
    let mut reg = ChannelRegistry::new();
    reg.register(Arc::new(MockChannel::new(vec![])));
    let kinds: Vec<&str> = reg.iter().map(|(k, _, _)| k).collect();
    assert_eq!(kinds, vec!["mock"]);
}

#[test]
fn registry_iter_yields_both_slugs_for_same_kind() {
    let mut reg = ChannelRegistry::new();
    reg.register(Arc::new(MockChannel::with_slug(vec![], "a@example.com")));
    reg.register(Arc::new(MockChannel::with_slug(vec![], "b@example.com")));
    let mut slugs: Vec<&str> = reg.iter().map(|(_, s, _)| s).collect();
    slugs.sort_unstable();
    assert_eq!(slugs, vec!["a@example.com", "b@example.com"]);
}

#[test]
fn channel_error_display() {
    let e = ChannelError::Config("missing host".into());
    assert!(e.to_string().contains("missing host"));
    let e2 = ChannelError::UnauthorizedSender("attacker@example.com".into());
    assert!(e2.to_string().contains("attacker@example.com"));
}

#[test]
fn outbound_delivery_failure_classification_separates_pressure_from_rejection() {
    assert_eq!(
        ChannelError::Transport("connection reset".into()).delivery_failure_class(),
        DeliveryFailureClass::Transient
    );
    assert_eq!(
        ChannelError::RetryableAuth("minted token rejected".into()).delivery_failure_class(),
        DeliveryFailureClass::Transient
    );
    assert_eq!(
        ChannelError::PermanentTransport("550 recipient rejected".into()).delivery_failure_class(),
        DeliveryFailureClass::Permanent
    );
    for error in [
        ChannelError::Config("missing host".into()),
        ChannelError::Auth("invalid credentials".into()),
        ChannelError::UnauthorizedSender("denied".into()),
        ChannelError::InvalidEnvelope("bad recipient".into()),
    ] {
        assert_eq!(
            error.delivery_failure_class(),
            DeliveryFailureClass::Permanent,
            "static failure must not remain in a raw-cadence retry loop: {error}"
        );
    }
}

#[test]
fn new_thread_correlation_id_is_uuid() {
    let id = new_thread_correlation_id();
    assert!(
        id.parse::<Uuid>().is_ok(),
        "correlation id must be a valid UUID"
    );
}

#[test]
fn is_configured_default_returns_true() {
    let ch = MockChannel::new(vec![]);
    // Default impl must return true; concrete adapters may override.
    assert!(
        ch.is_configured(),
        "default is_configured() must return true"
    );
}

#[tokio::test]
async fn default_poll_page_wraps_legacy_poll_without_checkpoint() {
    // #449: an adapter that implements only `poll` (the pre-checkpoint
    // shape) must still work through `poll_page`, returning the same
    // envelopes with no checkpoint to persist.
    let inbound =
        vec![
            ChannelEnvelope::new("email:sender@example.com", "email:me@example.com", "body")
                .with_external_id("<id1@example.com>"),
        ];
    let ch = MockChannel::new(inbound.clone());
    let page = ch.poll_page(Utc::now(), None).await.expect("poll_page ok");
    assert_eq!(page.envelopes.len(), 1);
    assert_eq!(
        page.envelopes[0].external_id.as_deref(),
        Some("<id1@example.com>")
    );
    assert!(
        page.next_checkpoint.is_none(),
        "default poll_page must never produce a checkpoint"
    );
}

#[tokio::test]
async fn mock_channel_send_and_poll() {
    let inbound =
        vec![
            ChannelEnvelope::new("email:sender@example.com", "email:me@example.com", "body")
                .with_external_id("<id1@example.com>"),
        ];
    let ch = Arc::new(MockChannel::new(inbound.clone()));
    let env_out = ChannelEnvelope::new("email:me@example.com", "email:them@example.com", "reply");
    ch.send(env_out).await.expect("send ok");
    assert_eq!(ch.sent.lock().unwrap().len(), 1);

    let polled = ch.poll(Utc::now()).await.expect("poll ok");
    assert_eq!(polled.len(), 1);
    assert_eq!(polled[0].external_id.as_deref(), Some("<id1@example.com>"));
}
