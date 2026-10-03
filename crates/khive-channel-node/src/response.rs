use crate::encoding::{CanonicalUuid, JsonInteger};
use crate::plaintext::PlaintextClassification;
use crate::receipt::{WireReceipt, WireReceiptBinding};
use crate::wire::{
    ContactResponse, Delivery, MessageState, ReceiptItem, RefusalCode, ServerTimestamp,
    WireDecodeError,
};
use crate::ProtocolError;
use khive_channel::ChannelError;

#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error("{error}")]
    Channel {
        error: ChannelError,
        diagnostic: Option<RemoteDiagnostic>,
    },
    #[error(transparent)]
    Source(#[from] crate::source::SourceError),
    #[error(transparent)]
    Pins(#[from] crate::pins::PinSourceError),
}
impl NodeError {
    pub(crate) fn invalid(message: &str) -> Self {
        Self::Channel {
            error: ChannelError::InvalidEnvelope(message.into()),
            diagnostic: None,
        }
    }
    pub(crate) fn transport(message: &str) -> Self {
        Self::Channel {
            error: ChannelError::Transport(message.into()),
            diagnostic: None,
        }
    }
    pub fn channel_error(&self) -> Option<&ChannelError> {
        match self {
            Self::Channel { error, .. } => Some(error),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockDiagnosis {
    MoreThan300SecondsBehind,
    AtLeast60SecondsAhead,
    WithinSkewBounds,
    DateUnavailable,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteDiagnostic {
    pub status: u16,
    pub date: Option<String>,
    pub clock: Option<ClockDiagnosis>,
    pub retry_after_seconds: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeldReason {
    WrongRecipient,
    NonContact,
    UnconfirmedSenderEpoch,
    FingerprintMismatch,
    PinUnavailable,
    EnvelopeDidNotAuthenticate,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeliveryOpenResult {
    Opened(PlaintextClassification),
    HeldUnopened(HeldReason),
}
#[derive(Debug)]
pub struct NodeDelivery {
    pub(crate) index: usize,
    pub(crate) delivery: Delivery,
    pub(crate) original: Vec<u8>,
    pub(crate) opening: DeliveryOpenResult,
}
impl NodeDelivery {
    pub fn index(&self) -> usize {
        self.index
    }
    pub fn delivery(&self) -> &Delivery {
        &self.delivery
    }
    pub fn original_json(&self) -> &[u8] {
        &self.original
    }
    pub fn opening(&self) -> &DeliveryOpenResult {
        &self.opening
    }
    /// Only opened envelopes can supply a binding for the caller's later committed receipt.
    pub fn receipt_binding(&self) -> Result<WireReceiptBinding, ProtocolError> {
        if !matches!(self.opening, DeliveryOpenResult::Opened(_)) {
            return Err(ProtocolError::Decryption);
        }
        // Parse the preserved object itself; serialization is never a receipt-binding source.
        let d = Delivery::parse(&self.original).map_err(|error| match error {
            WireDecodeError::Protocol(error) => error,
            WireDecodeError::Json(_) => ProtocolError::InvalidEncoding,
        })?;
        Ok(WireReceiptBinding {
            protocol_version: d.protocol_version,
            logical_message_id: d.logical_message_id,
            sender_agent_id: d.sender_agent_id,
            recipient_agent_id: d.recipient_agent_id,
            recipient_device_id: d.recipient_device_id,
            recipient_key_epoch: d.recipient_key_epoch,
            contact_generation: d.contact_generation,
            delivery_attempt_id: d.delivery_attempt_id,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReceiptRejection {
    SourceMissing,
    SourceUnavailable,
    SourceMismatch,
    BindingMismatch,
    PinUnavailable,
    PinUnconfirmed,
    FingerprintMismatch,
    InvalidSignature,
}
#[derive(Debug)]
pub struct VerifiedSenderReceipt {
    pub(crate) receipt: WireReceipt,
}
impl VerifiedSenderReceipt {
    pub fn receipt(&self) -> &WireReceipt {
        &self.receipt
    }
}
#[derive(Debug)]
pub enum ReceiptVerification {
    Verified(VerifiedSenderReceipt),
    Rejected(ReceiptRejection),
}
#[derive(Debug)]
pub struct NodeReceiptResult {
    pub index: usize,
    pub item: ReceiptItem,
    pub verification: ReceiptVerification,
}

/// One refused wire item, retaining its page position and original JSON bytes.
#[derive(Debug)]
pub struct NodePollRejection {
    pub index: usize,
    pub code: RefusalCode,
    pub(crate) original: Vec<u8>,
}
impl NodePollRejection {
    pub fn original_json(&self) -> &[u8] {
        &self.original
    }
}

#[derive(Debug)]
pub struct NodePollResult {
    pub deliveries: Vec<NodeDelivery>,
    pub receipts: Vec<NodeReceiptResult>,
    pub rejected_deliveries: Vec<NodePollRejection>,
    pub rejected_receipts: Vec<NodePollRejection>,
    pub next_cursor: JsonInteger,
    pub server_time: ServerTimestamp,
}
#[derive(Debug)]
pub struct NodeStatusResult {
    pub logical_message_id: CanonicalUuid,
    pub state: MessageState,
    pub receipt: Option<ReceiptVerification>,
}
#[derive(Debug)]
pub struct NodeContactResult {
    pub observed: ContactResponse,
    pub matches_owner_pin: bool,
}
