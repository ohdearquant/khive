//! Receipt-aware channel adapter over the bound node client.

use crate::client::NodeClient;
use crate::encoding::{CanonicalUuid, JsonInteger, NodeAddress, PollWait, Realm};
use crate::plaintext::{InvalidPlaintextReason, PlaintextClassification, PlaintextKind};
use crate::receipt::WireReceipt;
use crate::response::{
    DeliveryOpenResult, NodeDelivery, NodeError, NodePollResult, ReceiptVerification,
};
use crate::source::NodeClientBinding;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use khive_channel::{
    Channel, ChannelEnvelope, ChannelError, ChannelPollPage, DeliveryPage, DeliveryReceipt,
    InboundReceiptTicket, ReceiptRejectionReason, SendOutcome, SenderReceiptClaim,
    SenderReceiptResult, StoredChannelCheckpoint, LOGICAL_MESSAGE_ID_METADATA_KEY,
};

/// Sender receipt sequences never reset for a node device.
pub const NODE_RECEIPT_CURSOR_GENERATION: u64 = 1;

/// Inbound metadata: the kind a protocol-v1 message declares (`announce`, `report` or `ask`).
/// Absent when the plaintext declares none.
pub const MESSAGE_KIND_METADATA_KEY: &str = "khive.message_kind";
/// Inbound metadata: the logical message id the plaintext names in `in_reply_to`.
pub const IN_REPLY_TO_METADATA_KEY: &str = "khive.in_reply_to";
/// Inbound metadata: why an authenticated delivery's plaintext is invalid (ADR-105 A.8, Receiving,
/// step 4). An envelope carrying it holds no message: it is quarantined, never stored.
pub const INVALID_PLAINTEXT_METADATA_KEY: &str = "khive.invalid_plaintext";

/// The durable receipt cursor source binds the exact realm, agent and device.
pub fn receipt_cursor_source(binding: &NodeClientBinding) -> String {
    format!(
        "khive-node-v1-receipts:{}/{}/{}",
        binding.realm.as_str(),
        binding.agent,
        binding.device
    )
}

/// A stateless receipt cursor adapter. The caller supplies durable progress on every poll.
pub struct NodeChannel {
    client: NodeClient,
    wait: PollWait,
    source: String,
}

impl NodeChannel {
    /// PollWait has already validated the protocol's zero-to-25-second range.
    pub fn new(client: NodeClient, wait: PollWait) -> Self {
        let source = receipt_cursor_source(client.binding());
        Self {
            client,
            wait,
            source,
        }
    }

    fn requested_cursor(
        &self,
        stored: Option<&StoredChannelCheckpoint>,
    ) -> Result<u64, ChannelError> {
        let Some(stored) = stored.filter(|stored| stored.checkpoint.source == self.source) else {
            return Ok(0);
        };
        if stored.checkpoint.generation != NODE_RECEIPT_CURSOR_GENERATION {
            return Err(ChannelError::Config(
                "node receipt checkpoint generation mismatch".into(),
            ));
        }
        Ok(stored.checkpoint.high_water.unwrap_or(0))
    }
}

#[async_trait]
impl Channel for NodeChannel {
    fn kind(&self) -> &'static str {
        "khive"
    }
    fn slug(&self) -> String {
        self.client.binding().slug.clone()
    }

    async fn send(&self, _envelope: ChannelEnvelope) -> Result<(), ChannelError> {
        Err(ChannelError::Config(
            "khive requires receipt-aware submission".into(),
        ))
    }

    async fn poll(&self, _since: DateTime<Utc>) -> Result<Vec<ChannelEnvelope>, ChannelError> {
        Err(ChannelError::Config(
            "khive requires receipt-aware polling".into(),
        ))
    }

    async fn send_with_receipt(
        &self,
        envelope: ChannelEnvelope,
    ) -> Result<SendOutcome, ChannelError> {
        let raw = envelope
            .metadata
            .get(LOGICAL_MESSAGE_ID_METADATA_KEY)
            .ok_or_else(|| {
                ChannelError::InvalidEnvelope("missing logical message id metadata".into())
            })?;
        let logical = CanonicalUuid::parse(raw).map_err(|_| {
            ChannelError::InvalidEnvelope("invalid logical message id metadata".into())
        })?;
        self.client
            .submit(logical.into_uuid())
            .await
            .map_err(map_node_error)
    }

    /// Returns every delivery on the page that opened as an envelope with its receipt ticket,
    /// beside the page's sender receipt results and its receipt cursor. A delivery that did not
    /// open, or that names another recipient, gets no envelope and no receipt (ADR-105 A.8,
    /// Receiving, steps 1 to 3); it is not acknowledged, so the service hands it out again.
    /// The caller commits the cursor only when no result on the page is
    /// `SenderReceiptResult::Unhandled` (amendment of 2026-10-03, items 3 and 4).
    async fn poll_deliveries(
        &self,
        _since: DateTime<Utc>,
        stored: Option<&StoredChannelCheckpoint>,
    ) -> Result<DeliveryPage, ChannelError> {
        let requested = self.requested_cursor(stored)?;
        let cursor = JsonInteger::new(requested).map_err(|_| {
            ChannelError::Config("node receipt checkpoint exceeds protocol bounds".into())
        })?;
        let mut page = self
            .client
            .poll(cursor, self.wait)
            .await
            .map_err(map_node_error)?;
        let returned = page.next_cursor.get();
        if returned < requested {
            return Err(ChannelError::Transport(format!(
                "node receipt cursor {returned} is below requested receipts_after {requested}"
            )));
        }
        let realm = &self.client.binding().realm;
        let (envelopes, tickets): (Vec<_>, Vec<_>) = std::mem::take(&mut page.deliveries)
            .iter()
            .filter_map(|delivery| inbound_envelope(realm, delivery))
            .map(|(envelope, ticket)| (envelope, Some(ticket)))
            .unzip();
        DeliveryPage::new_with_receipts(
            ChannelPollPage::stateless(envelopes),
            tickets,
            sender_receipt_results(page),
            Some(returned),
        )
    }

    async fn acknowledge_receipt(&self, receipt: &DeliveryReceipt) -> Result<(), ChannelError> {
        let wire = WireReceipt::from_channel(receipt).map_err(|_| {
            ChannelError::InvalidEnvelope("invalid node acknowledgement receipt".into())
        })?;
        map_acknowledgement_result(self.client.ack(&wire).await)
    }
}

/// One opened delivery as an inbound envelope and the ticket for its later receipt. The envelope
/// keeps the delivery item verbatim in its replay context, because a quarantine record keeps it
/// whether the plaintext is invalid or the pair policy refuses a valid one (A.8, step 4).
fn inbound_envelope(
    realm: &Realm,
    delivery: &NodeDelivery,
) -> Option<(ChannelEnvelope, InboundReceiptTicket)> {
    let DeliveryOpenResult::Opened(classification) = delivery.opening() else {
        return None;
    };
    // The binding is re-parsed from the original bytes the envelope opened under; without
    // one no receipt can be signed, so the delivery is left to be handed out again.
    let binding = delivery.receipt_binding().ok()?;
    let d = delivery.delivery();
    let from = NodeAddress::new(realm.clone(), d.sender_agent_id).to_string();
    let to = NodeAddress::new(realm.clone(), d.recipient_agent_id).to_string();
    let mut envelope = match classification {
        PlaintextClassification::Valid(plaintext) => {
            let mut envelope = ChannelEnvelope::new(from.clone(), to, plaintext.body.clone());
            envelope.subject = plaintext.subject.clone();
            envelope.sent_at = Some(plaintext.sent_at.as_utc());
            envelope.correlation_external_id = plaintext.thread_id.map(|id| id.to_string());
            if let Some(kind) = plaintext.kind {
                envelope
                    .metadata
                    .insert(MESSAGE_KIND_METADATA_KEY.into(), kind_name(kind).into());
            }
            if let Some(parent) = plaintext.in_reply_to {
                envelope
                    .metadata
                    .insert(IN_REPLY_TO_METADATA_KEY.into(), parent.to_string());
            }
            envelope
        }
        PlaintextClassification::Invalid(reason) => {
            let mut envelope = ChannelEnvelope::new(from.clone(), to, String::new());
            envelope.metadata.insert(
                INVALID_PLAINTEXT_METADATA_KEY.into(),
                invalid_reason_name(*reason).into(),
            );
            envelope
        }
    };
    envelope.metadata.insert(
        LOGICAL_MESSAGE_ID_METADATA_KEY.into(),
        d.logical_message_id.to_string(),
    );
    let envelope = envelope.with_quarantine_replay(delivery.original_json().to_vec(), from);
    let ticket = InboundReceiptTicket::new(binding.to_channel(), d.sender_key_epoch.get());
    Some((envelope, ticket))
}

fn kind_name(kind: PlaintextKind) -> &'static str {
    match kind {
        PlaintextKind::Announce => "announce",
        PlaintextKind::Report => "report",
        PlaintextKind::Ask => "ask",
    }
}

fn invalid_reason_name(reason: InvalidPlaintextReason) -> &'static str {
    match reason {
        InvalidPlaintextReason::NotObject => "not_object",
        InvalidPlaintextReason::DuplicateMember => "duplicate_member",
        InvalidPlaintextReason::ReservedIdentityMember => "reserved_identity_member",
        InvalidPlaintextReason::InvalidKind => "invalid_kind",
    }
}

fn sender_receipt_results(page: NodePollResult) -> Vec<SenderReceiptResult> {
    let mut results = Vec::with_capacity(page.receipts.len() + page.rejected_receipts.len());
    for result in page.receipts {
        let binding = &result.item.receipt.binding;
        let claim = SenderReceiptClaim {
            logical_message_id: Some(binding.logical_message_id.into_uuid()),
            recipient_device_id: Some(binding.recipient_device_id.into_uuid()),
            recipient_key_epoch: Some(binding.recipient_key_epoch.get()),
        };
        let value = match result.verification {
            ReceiptVerification::Verified(verified) => {
                SenderReceiptResult::Verified(verified.into_verified())
            }
            ReceiptVerification::Rejected(reason) => {
                SenderReceiptResult::Rejected { claim, reason }
            }
            ReceiptVerification::Unhandled(reason) => {
                SenderReceiptResult::Unhandled { claim, reason }
            }
        };
        results.push((result.index, value));
    }
    for result in page.rejected_receipts {
        results.push((
            result.index,
            SenderReceiptResult::Rejected {
                claim: SenderReceiptClaim::default(),
                reason: ReceiptRejectionReason::ParseFailure,
            },
        ));
    }
    results.sort_by_key(|(index, _)| *index);
    results.into_iter().map(|(_, value)| value).collect()
}

fn map_node_error(error: NodeError) -> ChannelError {
    match error {
        NodeError::Channel { error, .. } => error,
        NodeError::Source(error) => ChannelError::Transport(error.to_string()),
        NodeError::Pins(error) => ChannelError::Transport(error.to_string()),
    }
}

// Keep this seam separate from the later typed, unsigned journal acknowledgement API.
fn map_acknowledgement_result(result: Result<(), NodeError>) -> Result<(), ChannelError> {
    result.map_err(map_node_error)
}
