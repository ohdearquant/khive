//! Persisted outbound submissions read through the runtime.

use async_trait::async_trait;
use khive_channel_node::encoding::{
    Base64Bytes, CanonicalUuid, Epoch, HexBytes, NodeAddress, ProtocolVersion,
};
use khive_channel_node::envelope::Ciphertext;
use khive_channel_node::source::{
    NodeClientBinding, OutboundSource, PersistedSubmission, SourceError,
};
use khive_runtime::comm_transport::{EnvelopeKey, SenderRecord};
use khive_runtime::{KhiveRuntime, NamespaceToken};
use uuid::Uuid;

const KIND: &str = "khive";

fn unavailable(detail: &str) -> SourceError {
    SourceError(detail.to_string())
}

/// Reads this node's submissions from the comm-routed runtime, bound to one namespace
/// token and the enrolled sender binding.
pub(crate) struct RuntimeOutboundSource {
    runtime: KhiveRuntime,
    token: NamespaceToken,
    binding: NodeClientBinding,
}

impl RuntimeOutboundSource {
    pub(crate) fn new(
        runtime: KhiveRuntime,
        token: NamespaceToken,
        binding: NodeClientBinding,
    ) -> Self {
        Self {
            runtime,
            token,
            binding,
        }
    }

    /// Validate a stored row through the protocol wrappers and bind the sender device from
    /// config. Refuses a row whose sender agent, epoch or credential reference differs from
    /// the enrolled binding; an old row is never rewritten to fit new configuration.
    pub(crate) fn convert(&self, row: SenderRecord) -> Result<PersistedSubmission, SourceError> {
        let e = row.envelope;
        if e.namespace != self.binding.namespace || e.kind != KIND || e.slug != self.binding.slug {
            return Err(unavailable("stored submission is outside the node binding"));
        }
        let sender_agent = CanonicalUuid::parse(&e.sender_agent_id)
            .map_err(|_| unavailable("stored sender agent is invalid"))?;
        let sender_key_epoch = Epoch::new(e.sender_key_epoch)
            .map_err(|_| unavailable("stored sender epoch is invalid"))?;
        if sender_agent != self.binding.agent
            || sender_key_epoch != self.binding.key_epoch
            || e.credential_ref != self.binding.key_reference
        {
            return Err(unavailable(
                "stored sender does not match the enrolled binding",
            ));
        }
        let enc: [u8; 32] = <[u8; 32]>::try_from(e.enc.as_slice())
            .map_err(|_| unavailable("stored enc length is invalid"))?;
        Ok(PersistedSubmission {
            namespace: e.namespace,
            kind: e.kind,
            slug: e.slug,
            key_reference: e.credential_ref,
            protocol_version: ProtocolVersion::new(u64::from(e.protocol_version))
                .map_err(|_| unavailable("stored protocol version is invalid"))?,
            logical_message_id: CanonicalUuid::from_uuid(e.logical_message_id),
            sender_agent,
            sender_device: self.binding.device,
            sender_key_epoch,
            recipient: NodeAddress::parse(&e.recipient_address)
                .map_err(|_| unavailable("stored recipient address is invalid"))?,
            recipient_agent: CanonicalUuid::parse(&e.recipient_agent_id)
                .map_err(|_| unavailable("stored recipient agent is invalid"))?,
            recipient_device: CanonicalUuid::from_uuid(e.recipient_device_id),
            recipient_key_epoch: Epoch::new(e.recipient_key_epoch)
                .map_err(|_| unavailable("stored recipient epoch is invalid"))?,
            recipient_fingerprint: HexBytes::<32>::parse(&e.recipient_key_fingerprint)
                .map_err(|_| unavailable("stored recipient fingerprint is invalid"))?,
            contact_generation: Epoch::new(e.contact_generation)
                .map_err(|_| unavailable("stored contact generation is invalid"))?,
            enc: Base64Bytes::<32>::new(enc),
            ciphertext: Ciphertext::new(e.ciphertext)
                .map_err(|_| unavailable("stored ciphertext is invalid"))?,
        })
    }
}

#[async_trait]
impl OutboundSource for RuntimeOutboundSource {
    async fn current(&self, logical_id: Uuid) -> Result<Option<PersistedSubmission>, SourceError> {
        self.runtime
            .current_sender_transport(&self.token, KIND, &self.binding.slug, logical_id)
            .await
            .map_err(|_| unavailable("outbound read failed"))?
            .map(|row| self.convert(row))
            .transpose()
    }

    async fn for_receipt(
        &self,
        logical_id: Uuid,
        recipient_device_id: Uuid,
        recipient_key_epoch: u64,
    ) -> Result<Option<PersistedSubmission>, SourceError> {
        let row = self
            .runtime
            .sender_transport(EnvelopeKey {
                logical_message_id: logical_id,
                recipient_device_id,
                recipient_key_epoch,
            })
            .await
            .map_err(|_| unavailable("outbound history read failed"))?;
        let Some(row) = row else {
            return Ok(None);
        };
        // The exact-key query is global; a row from another namespace, kind or slug is not
        // this source's submission.
        if row.envelope.namespace != self.binding.namespace
            || row.envelope.kind != KIND
            || row.envelope.slug != self.binding.slug
        {
            return Ok(None);
        }
        self.convert(row).map(Some)
    }
}
