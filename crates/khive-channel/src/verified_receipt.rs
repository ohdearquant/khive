//! Signature-verified sender receipt values shared by adapters and the runtime.

use crate::{receipt_signing_input, DeliveryReceipt, ReceiptSigningPublicKey};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ReceiptVerificationError {
    #[error("receipt agent identifier is not a canonical UUID")]
    InvalidAgentId,
    #[error("receipt signature must contain exactly 64 bytes")]
    InvalidSignatureLength,
    #[error("recipient receipt signature is invalid")]
    InvalidSignature,
}

/// A receipt whose signature was checked with the supplied pinned recipient key.
/// The adapter must obtain that key from the owner's pin for the recorded epoch.
/// This value proves a signature check, not the provenance of the supplied key.
///
/// The payload cannot be constructed outside this crate:
///
/// ```compile_fail
/// use khive_channel::{DeliveryReceipt, VerifiedRecipientReceipt};
/// fn forge(receipt: DeliveryReceipt) {
///     let _ = VerifiedRecipientReceipt { 0: receipt.clone() };
///     let _ = VerifiedRecipientReceipt(receipt);
/// }
/// ```
///
/// There is no default path around verification:
///
/// ```compile_fail
/// use khive_channel::VerifiedRecipientReceipt;
/// let _: VerifiedRecipientReceipt = Default::default();
/// ```
///
/// Deserializing wire data cannot assert verification:
///
/// ```compile_fail
/// use khive_channel::VerifiedRecipientReceipt;
/// fn decode(json: &str) {
///     let _: VerifiedRecipientReceipt = serde_json::from_str(json).unwrap();
/// }
/// ```
///
/// These refusal examples have this successful twin: an unverified receipt can
/// round-trip through JSON, then the constructor checks its signature.
///
/// ```
/// use khive_channel::{receipt_signing_input, DeliveryReceipt, DeliveryReceiptBinding,
///     ReceiptDisposition, SendOutcome, VerifiedRecipientReceipt};
/// use ring::signature::{Ed25519KeyPair, KeyPair};
/// use uuid::Uuid;
/// let key = Ed25519KeyPair::from_seed_unchecked(&[7; 32]).unwrap();
/// let mut receipt = DeliveryReceipt {
///     binding: DeliveryReceiptBinding {
///         protocol_version: 1,
///         logical_message_id: Uuid::nil(),
///         sender_agent_id: Uuid::nil().to_string(),
///         recipient_agent_id: Uuid::nil().to_string(),
///         recipient_device_id: Uuid::nil(),
///         recipient_key_epoch: 1,
///         contact_generation: 1,
///         delivery_attempt_id: Uuid::nil(),
///     },
///     disposition: ReceiptDisposition::Stored,
///     signature: Vec::new(),
/// };
/// receipt.signature = key.sign(&receipt_signing_input(&receipt).unwrap()).as_ref().to_vec();
/// let decoded: DeliveryReceipt = serde_json::from_str(&serde_json::to_string(&receipt).unwrap())
///     .unwrap();
/// let pinned_key: [u8; 32] = key.public_key().as_ref().try_into().unwrap();
/// let verified = VerifiedRecipientReceipt::verify(decoded, &pinned_key).unwrap();
/// assert_eq!(verified.receipt(), &receipt);
/// assert!(SendOutcome::RecipientStored(verified).validate_receipt().is_ok());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedRecipientReceipt(DeliveryReceipt);

impl VerifiedRecipientReceipt {
    /// Verify with the owner's signing public key for the recorded recipient epoch.
    pub fn verify(
        receipt: DeliveryReceipt,
        pinned_signing_public_key: &[u8; 32],
    ) -> Result<Self, ReceiptVerificationError> {
        if receipt.signature.len() != 64 {
            return Err(ReceiptVerificationError::InvalidSignatureLength);
        }
        let input = receipt_signing_input(&receipt)
            .map_err(|_| ReceiptVerificationError::InvalidAgentId)?;
        let signing_key = ReceiptSigningPublicKey::new(*pinned_signing_public_key)
            .map_err(|_| ReceiptVerificationError::InvalidSignature)?;
        let signature = receipt
            .signature
            .as_slice()
            .try_into()
            .map_err(|_| ReceiptVerificationError::InvalidSignatureLength)?;
        signing_key
            .verify(&input, signature)
            .map_err(|_| ReceiptVerificationError::InvalidSignature)?;
        Ok(Self(receipt))
    }

    pub fn receipt(&self) -> &DeliveryReceipt {
        &self.0
    }
}

#[cfg(test)]
#[path = "verified_receipt_tests.rs"]
mod tests;
