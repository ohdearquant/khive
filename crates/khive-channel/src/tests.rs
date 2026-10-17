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

include!("tests/legacy_receipt_defaults_tests.rs");

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

include!("tests/delivery_receipt_validation_tests.rs");

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

include!("tests/adapter_api_tests.rs");
