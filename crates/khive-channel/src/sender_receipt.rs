//! Sender receipt results carried independently of inbound envelope checkpoints.

use crate::VerifiedRecipientReceipt;
use uuid::Uuid;

/// Outbound metadata identifying the persisted node submission to send.
pub const LOGICAL_MESSAGE_ID_METADATA_KEY: &str = "khive.logical_message_id";

/// Claimed identifiers, when available, from an unverified receipt item.
/// These identify a candidate row; they authorize no state change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SenderReceiptClaim {
    pub logical_message_id: Option<Uuid>,
    pub recipient_device_id: Option<Uuid>,
    pub recipient_key_epoch: Option<u64>,
}

/// A receipt-caused refusal that can be reported before advancing the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReceiptRejectionReason {
    ParseFailure,
    SourceMissing,
    SourceMismatch,
    BindingMismatch,
    PinUnconfirmed,
    FingerprintMismatch,
    InvalidSignature,
}

impl ReceiptRejectionReason {
    /// Stable spellings retained in an outbox row's unverified-receipt reason.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ParseFailure => "ParseFailure",
            Self::SourceMissing => "SourceMissing",
            Self::SourceMismatch => "SourceMismatch",
            Self::BindingMismatch => "BindingMismatch",
            Self::PinUnconfirmed => "PinUnconfirmed",
            Self::FingerprintMismatch => "FingerprintMismatch",
            Self::InvalidSignature => "InvalidSignature",
        }
    }
}

/// A local read failure leaves the receipt unhandled and its cursor uncommitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReceiptReadFailure {
    SourceUnavailable,
    PinUnavailable,
}

/// One result per sender receipt item, in its original page order.
#[derive(Debug)]
pub enum SenderReceiptResult {
    Verified(VerifiedRecipientReceipt),
    Rejected {
        claim: SenderReceiptClaim,
        reason: ReceiptRejectionReason,
    },
    Unhandled {
        claim: SenderReceiptClaim,
        reason: ReceiptReadFailure,
    },
}

#[cfg(test)]
#[path = "sender_receipt_tests.rs"]
mod tests;
