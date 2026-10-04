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
#[async_trait]
pub trait OutboundSource: Send + Sync {
    async fn current(&self, logical_id: Uuid) -> Result<Option<PersistedSubmission>, SourceError>;
}
