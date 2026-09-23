//! Runtime-owned sender transport persistence (ADR-105).
//!
//! Call these methods on the runtime bound to comm's assigned backend, exactly
//! as for ordinary comm note operations. Routing happens before this boundary;
//! the adapter never opens a database or supplies SQL. Opaque envelope bytes and
//! receipt signatures must be produced/verified by the trusted node caller.
use crate::error::RuntimeResult;
use crate::{KhiveRuntime, NamespaceToken};
use khive_channel::DeliveryReceipt;
use khive_db::stores::note::transport::SenderTransportStore;
pub use khive_db::stores::note::transport::{
    EnvelopeKey, FailureClass, HoldReason, SenderEnvelope, SenderRecord, TransportState,
};

/// An explicit assertion by the trusted caller that it verified the signature
/// against the pinned recipient key. This wrapper does not perform cryptography
/// and cannot be deserialized from a request parameter.
#[derive(Debug)]
pub struct VerifiedRecipientReceipt(DeliveryReceipt);
impl VerifiedRecipientReceipt {
    /// Construct only after cryptographically verifying the receipt. Binding and
    /// disposition checks run again atomically against durable submission data.
    pub fn after_signature_verification(receipt: DeliveryReceipt) -> Self {
        Self(receipt)
    }
}

impl KhiveRuntime {
    fn sender_transport_store(&self) -> SenderTransportStore {
        SenderTransportStore::new(self.backend().pool_arc())
    }
    /// Persist immutable bytes before first submission. An exact retry is a no-op;
    /// a different envelope with the same identity is refused. The namespace is
    /// caller attribution and is always taken from the token.
    pub async fn create_sender_transport(
        &self,
        token: &NamespaceToken,
        mut envelope: SenderEnvelope,
    ) -> RuntimeResult<SenderRecord> {
        envelope.namespace = token.namespace().as_str().to_owned();
        Ok(self
            .sender_transport_store()
            .create(envelope, false)
            .await?)
    }
    /// Re-encrypt only after the caller confirms the recipient key change. The
    /// old record must be on a key-change hold, and the new epoch must increase.
    /// Cryptographic construction and directory confirmation belong to the caller.
    pub async fn reencrypt_sender_transport_after_confirmed_key_change(
        &self,
        token: &NamespaceToken,
        mut envelope: SenderEnvelope,
    ) -> RuntimeResult<SenderRecord> {
        envelope.namespace = token.namespace().as_str().to_owned();
        Ok(self.sender_transport_store().create(envelope, true).await?)
    }
    /// Read by durable identity. Absence means unknown; unknown is not persisted.
    pub async fn sender_transport(&self, key: EnvelopeKey) -> RuntimeResult<Option<SenderRecord>> {
        Ok(self.sender_transport_store().get(key).await?)
    }
    /// Due unheld submissions for the exact configured kind and slug. Times use
    /// Unix microseconds; choosing the backoff and jitter is the caller's job.
    pub async fn pending_sender_transports(
        &self,
        token: &NamespaceToken,
        kind: &str,
        slug: &str,
        now: i64,
        limit: u32,
    ) -> RuntimeResult<Vec<SenderRecord>> {
        Ok(self
            .sender_transport_store()
            .list_pending(token.namespace().as_str(), kind, slug, now, limit)
            .await?)
    }
    /// Record a failed attempt. Authentication errors remain pending and do not
    /// advance the attempt count. This never modifies the envelope bytes.
    pub async fn record_sender_transport_failure(
        &self,
        key: EnvelopeKey,
        class: FailureClass,
        next_retry_at: Option<i64>,
    ) -> RuntimeResult<()> {
        Ok(self
            .sender_transport_store()
            .record_failure(key, class, next_retry_at)
            .await?)
    }
    /// Set/release a credit hold, or set a key-change hold. Key-change holds can
    /// only be superseded through explicit confirmed re-encryption.
    pub async fn hold_sender_transport(
        &self,
        key: EnvelopeKey,
        reason: Option<HoldReason>,
    ) -> RuntimeResult<()> {
        Ok(self.sender_transport_store().hold(key, reason).await?)
    }
    /// Accept an already signature-verified recipient receipt, including after a
    /// local failure. Every known binding field and target disposition must match
    /// durable data. This method performs no signature verification itself.
    pub async fn accept_verified_recipient_receipt(
        &self,
        key: EnvelopeKey,
        state: TransportState,
        receipt: VerifiedRecipientReceipt,
    ) -> RuntimeResult<()> {
        let receipt = serde_json::to_value(receipt.0).map_err(|error| {
            crate::RuntimeError::InvalidInput(format!("invalid receipt: {error}"))
        })?;
        Ok(self
            .sender_transport_store()
            .accept_receipt(key, state, receipt)
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use khive_channel::{DeliveryReceiptBinding, ReceiptDisposition};
    use uuid::Uuid;

    #[tokio::test]
    async fn verified_receipt_uses_bound_backend_and_token_attribution() {
        let runtime = KhiveRuntime::memory().unwrap();
        let unrelated = KhiveRuntime::memory().unwrap();
        let token = NamespaceToken::local();
        let envelope = SenderEnvelope {
            namespace: "untrusted-envelope-attribution".into(),
            logical_message_id: Uuid::new_v4(),
            outbound_note_id: Uuid::new_v4(),
            kind: "khive".into(),
            slug: "device".into(),
            credential_ref: "keys/device".into(),
            recipient_address: format!("khive1:example/{}", Uuid::nil()),
            protocol_version: 1,
            sender_agent_id: Uuid::new_v4().to_string(),
            recipient_agent_id: Uuid::nil().to_string(),
            recipient_device_id: Uuid::new_v4(),
            recipient_key_epoch: 1,
            contact_generation: 1,
            sender_key_epoch: 1,
            recipient_key_fingerprint: "ab".repeat(32),
            enc: vec![1; 32],
            ciphertext: vec![2, 0, 255],
        };
        let key = envelope.key();
        let stored = runtime
            .create_sender_transport(&token, envelope.clone())
            .await
            .unwrap();
        assert_eq!(stored.envelope.namespace, token.namespace().as_str());
        assert!(
            unrelated.sender_transport(key).await.unwrap().is_none(),
            "transport must use bound backend"
        );
        let receipt = DeliveryReceipt {
            binding: DeliveryReceiptBinding {
                protocol_version: 1,
                logical_message_id: envelope.logical_message_id,
                sender_agent_id: envelope.sender_agent_id,
                recipient_agent_id: envelope.recipient_agent_id,
                recipient_device_id: envelope.recipient_device_id,
                recipient_key_epoch: 1,
                contact_generation: 1,
                delivery_attempt_id: Uuid::new_v4(),
            },
            disposition: ReceiptDisposition::Stored,
            signature: vec![7],
        };
        assert!(runtime
            .accept_verified_recipient_receipt(
                key,
                TransportState::RecipientQuarantined,
                VerifiedRecipientReceipt::after_signature_verification(receipt.clone())
            )
            .await
            .is_err());
        runtime
            .accept_verified_recipient_receipt(
                key,
                TransportState::RecipientStored,
                VerifiedRecipientReceipt::after_signature_verification(receipt),
            )
            .await
            .unwrap();
        assert_eq!(
            runtime.sender_transport(key).await.unwrap().unwrap().state,
            TransportState::RecipientStored
        );
    }
}
