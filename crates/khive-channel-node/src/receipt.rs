//! Receipt wire encoding and private signing operations (ADR-105 A.6.4).
use crate::encoding::{Base64Bytes, CanonicalUuid, Epoch, ProtocolVersion};
use crate::keys::{KeyFacility, SigningPublicKey};
use crate::ProtocolError;
use khive_channel::{DeliveryReceipt, DeliveryReceiptBinding, ReceiptDisposition};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireReceiptBinding {
    pub protocol_version: ProtocolVersion,
    pub logical_message_id: CanonicalUuid,
    pub sender_agent_id: CanonicalUuid,
    pub recipient_agent_id: CanonicalUuid,
    pub recipient_device_id: CanonicalUuid,
    pub recipient_key_epoch: Epoch,
    pub contact_generation: Epoch,
    pub delivery_attempt_id: CanonicalUuid,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireReceipt {
    pub binding: WireReceiptBinding,
    pub disposition: ReceiptDisposition,
    pub signature: Base64Bytes<64>,
}
impl WireReceipt {
    pub fn to_channel(&self) -> DeliveryReceipt {
        let b = &self.binding;
        DeliveryReceipt {
            binding: DeliveryReceiptBinding {
                protocol_version: b.protocol_version.get() as u32,
                logical_message_id: b.logical_message_id.into_uuid(),
                sender_agent_id: b.sender_agent_id.to_string(),
                recipient_agent_id: b.recipient_agent_id.to_string(),
                recipient_device_id: b.recipient_device_id.into_uuid(),
                recipient_key_epoch: b.recipient_key_epoch.get(),
                contact_generation: b.contact_generation.get(),
                delivery_attempt_id: b.delivery_attempt_id.into_uuid(),
            },
            disposition: self.disposition,
            signature: self.signature.as_bytes().to_vec(),
        }
    }
    pub fn from_channel(receipt: &DeliveryReceipt) -> Result<Self, ProtocolError> {
        let b = &receipt.binding;
        Ok(Self {
            binding: WireReceiptBinding {
                protocol_version: ProtocolVersion::new(b.protocol_version.into())?,
                logical_message_id: CanonicalUuid::from_uuid(b.logical_message_id),
                sender_agent_id: CanonicalUuid::parse(&b.sender_agent_id)?,
                recipient_agent_id: CanonicalUuid::parse(&b.recipient_agent_id)?,
                recipient_device_id: CanonicalUuid::from_uuid(b.recipient_device_id),
                recipient_key_epoch: Epoch::new(b.recipient_key_epoch)?,
                contact_generation: Epoch::new(b.contact_generation)?,
                delivery_attempt_id: CanonicalUuid::from_uuid(b.delivery_attempt_id),
            },
            disposition: receipt.disposition,
            signature: Base64Bytes::new(
                receipt
                    .signature
                    .as_slice()
                    .try_into()
                    .map_err(|_| ProtocolError::InvalidSignature)?,
            ),
        })
    }
    pub fn signing_input(&self) -> Result<Vec<u8>, ProtocolError> {
        khive_channel::receipt_signing_input(&self.to_channel())
            .map_err(|_| ProtocolError::InvalidEncoding)
    }
    pub fn sign(
        binding: WireReceiptBinding,
        disposition: ReceiptDisposition,
        facility: &dyn KeyFacility,
    ) -> Result<Self, ProtocolError> {
        let mut receipt = Self {
            binding,
            disposition,
            signature: Base64Bytes::new([0; 64]),
        };
        receipt.signature = Base64Bytes::new(facility.sign(&receipt.signing_input()?));
        Ok(receipt)
    }
    pub fn verify(&self, pinned_recipient: &SigningPublicKey) -> Result<(), ProtocolError> {
        pinned_recipient.verify(&self.signing_input()?, self.signature.as_bytes())
    }
}
