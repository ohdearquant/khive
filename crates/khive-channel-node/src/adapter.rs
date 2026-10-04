//! Receipt-aware channel adapter over the bound node client.

use crate::client::NodeClient;
use crate::encoding::{CanonicalUuid, JsonInteger, PollWait};
use crate::receipt::WireReceipt;
use crate::response::{NodeError, NodePollResult, ReceiptVerification};
use crate::source::NodeClientBinding;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use khive_channel::{
    Channel, ChannelEnvelope, ChannelError, ChannelPollPage, DeliveryPage, DeliveryReceipt,
    ReceiptRejectionReason, SendOutcome, SenderReceiptClaim, SenderReceiptResult,
    StoredChannelCheckpoint, LOGICAL_MESSAGE_ID_METADATA_KEY,
};

/// Sender receipt sequences never reset for a node device.
pub const NODE_RECEIPT_CURSOR_GENERATION: u64 = 1;

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

    async fn poll_deliveries(
        &self,
        _since: DateTime<Utc>,
        stored: Option<&StoredChannelCheckpoint>,
    ) -> Result<DeliveryPage, ChannelError> {
        let requested = self.requested_cursor(stored)?;
        let cursor = JsonInteger::new(requested).map_err(|_| {
            ChannelError::Config("node receipt checkpoint exceeds protocol bounds".into())
        })?;
        let page = self
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
        DeliveryPage::new_with_receipts(
            ChannelPollPage::stateless(Vec::new()),
            Vec::new(),
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
