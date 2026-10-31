use crate::encoding::Base64Bytes;
use crate::encoding::{CanonicalUuid, Epoch, HexBytes, NodeAddress, ProtocolVersion, Realm};
use crate::envelope::Ciphertext;
use async_trait::async_trait;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeClientBinding {
    pub namespace: String,
    pub slug: String,
    pub realm: Realm,
    pub agent: CanonicalUuid,
    pub device: CanonicalUuid,
    pub key_epoch: Epoch,
    pub key_reference: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistedSubmission {
    pub namespace: String,
    pub kind: String,
    pub slug: String,
    pub key_reference: String,
    pub protocol_version: ProtocolVersion,
    pub logical_message_id: CanonicalUuid,
    pub sender_agent: CanonicalUuid,
    pub sender_device: CanonicalUuid,
    pub sender_key_epoch: Epoch,
    pub recipient: NodeAddress,
    pub recipient_agent: CanonicalUuid,
    pub recipient_device: CanonicalUuid,
    pub recipient_key_epoch: Epoch,
    pub recipient_fingerprint: HexBytes<32>,
    pub contact_generation: Epoch,
    pub enc: Base64Bytes<32>,
    pub ciphertext: Ciphertext,
}

impl PersistedSubmission {
    pub(crate) fn matches(&self, requested: Uuid, local: &NodeClientBinding) -> bool {
        self.logical_message_id.into_uuid() == requested
            && self.namespace == local.namespace
            && self.kind == "khive"
            && self.slug == local.slug
            && self.key_reference == local.key_reference
            && self.sender_agent == local.agent
            && self.sender_device == local.device
            && self.sender_key_epoch == local.key_epoch
            && self.recipient.require_realm(&local.realm).is_ok()
            && self.recipient.agent() == self.recipient_agent
    }
}

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
#[error("outbound source unavailable: {0}")]
pub struct SourceError(pub String);

/// Implementations resolve the current persisted envelope for their bound namespace and slug.
/// Keep the row's recipient epoch and fingerprint available through its receipt
/// retention; a later key rotation does not replace those recorded identifiers.
#[async_trait]
pub trait OutboundSource: Send + Sync {
    /// The current persisted envelope for a logical message: the highest envelope sequence.
    async fn current(&self, logical_id: Uuid) -> Result<Option<PersistedSubmission>, SourceError>;

    /// The persisted envelope for the exact recipient device and key epoch a receipt names.
    ///
    /// A recipient receipt may name an earlier key epoch than the current envelope after a
    /// confirmed key change. Sources that keep envelope history override this to return that
    /// exact historical row. The default is deliberately safe: it only answers when the current
    /// envelope already carries the requested device and epoch, so a source without history
    /// never verifies a receipt against a different envelope.
    async fn for_receipt(
        &self,
        logical_id: Uuid,
        recipient_device_id: Uuid,
        recipient_key_epoch: u64,
    ) -> Result<Option<PersistedSubmission>, SourceError> {
        Ok(self.current(logical_id).await?.filter(|p| {
            p.recipient_device.into_uuid() == recipient_device_id
                && p.recipient_key_epoch.get() == recipient_key_epoch
        }))
    }
}
