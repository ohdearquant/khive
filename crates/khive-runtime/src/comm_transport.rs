//! Runtime-owned sender transport persistence (ADR-105).
//!
//! Call these methods on the runtime bound to comm's assigned backend, exactly
//! as for ordinary comm note operations. Routing happens before this boundary;
//! the adapter never opens a database or supplies SQL. Receipt signatures are
//! checked against the owner's pinned recipient key before durable state changes.
use crate::error::RuntimeResult;
use crate::{KhiveRuntime, NamespaceToken};
use khive_channel::DeliveryReceipt;
use khive_db::stores::note::transport::SenderTransportStore;
pub use khive_db::stores::note::transport::{
    EnvelopeKey, FailureClass, HoldReason, PolicyMode, SenderAssurance, SenderEnvelope,
    SenderRecord, TransportState,
};
use uuid::Uuid;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ReceiptVerificationError {
    #[error("receipt agent identifier is not a canonical UUID")]
    InvalidAgentId,
    #[error("receipt signature must contain exactly 64 bytes")]
    InvalidSignatureLength,
    #[error("recipient receipt signature is invalid")]
    InvalidSignature,
}

fn canonical_agent_id(value: &str) -> Result<Uuid, ReceiptVerificationError> {
    let id = Uuid::parse_str(value).map_err(|_| ReceiptVerificationError::InvalidAgentId)?;
    if id.to_string() != value {
        return Err(ReceiptVerificationError::InvalidAgentId);
    }
    Ok(id)
}

fn receipt_signing_input(receipt: &DeliveryReceipt) -> Result<Vec<u8>, ReceiptVerificationError> {
    let binding = &receipt.binding;
    let sender_agent_id = canonical_agent_id(&binding.sender_agent_id)?;
    let recipient_agent_id = canonical_agent_id(&binding.recipient_agent_id)?;
    let mut input = b"khive-node-v1/receipt\0".to_vec();
    input.extend_from_slice(&binding.protocol_version.to_be_bytes());
    input.extend_from_slice(binding.logical_message_id.as_bytes());
    input.extend_from_slice(sender_agent_id.as_bytes());
    input.extend_from_slice(recipient_agent_id.as_bytes());
    input.extend_from_slice(binding.recipient_device_id.as_bytes());
    input.extend_from_slice(&binding.recipient_key_epoch.to_be_bytes());
    input.extend_from_slice(&binding.contact_generation.to_be_bytes());
    input.extend_from_slice(binding.delivery_attempt_id.as_bytes());
    input.push(match receipt.disposition {
        khive_channel::ReceiptDisposition::Stored => 1,
        khive_channel::ReceiptDisposition::Quarantined => 2,
    });
    Ok(input)
}

/// A receipt whose signature was checked with the owner's pinned recipient key.
/// The private payload prevents callers from asserting verification themselves.
#[derive(Debug)]
pub struct VerifiedRecipientReceipt(DeliveryReceipt);
impl VerifiedRecipientReceipt {
    /// Verify the receipt with the signing public key pinned by the owner for
    /// this contact at the receipt's recipient key epoch.
    pub fn verify(
        receipt: DeliveryReceipt,
        pinned_signing_public_key: &[u8; 32],
    ) -> Result<Self, ReceiptVerificationError> {
        if receipt.signature.len() != 64 {
            return Err(ReceiptVerificationError::InvalidSignatureLength);
        }
        let input = receipt_signing_input(&receipt)?;
        ring::signature::UnparsedPublicKey::new(
            &ring::signature::ED25519,
            pinned_signing_public_key,
        )
        .verify(&input, &receipt.signature)
        .map_err(|_| ReceiptVerificationError::InvalidSignature)?;
        Ok(Self(receipt))
    }
}

impl KhiveRuntime {
    fn sender_transport_store(&self) -> SenderTransportStore {
        SenderTransportStore::new(self.backend().pool_arc())
    }
    /// Persist immutable bytes before first submission. An exact retry is a no-op;
    /// a different envelope with the same identity is refused. The namespace is
    /// caller attribution and is always taken from the token. Sender assurance
    /// is claimed until authenticated verification is available.
    pub async fn create_sender_transport(
        &self,
        token: &NamespaceToken,
        mut envelope: SenderEnvelope,
    ) -> RuntimeResult<SenderRecord> {
        envelope.namespace = token.namespace().as_str().to_owned();
        envelope.sender_assurance = SenderAssurance::Claimed;
        Ok(self
            .sender_transport_store()
            .create(envelope, false)
            .await?)
    }
    /// Re-encrypt only after the caller confirms the recipient key change. The
    /// old record must be on a key-change hold, and the new epoch must increase.
    /// Cryptographic construction and directory confirmation belong to the caller.
    /// Caller-supplied assurance is normalized to claimed as on initial creation.
    pub async fn reencrypt_sender_transport_after_confirmed_key_change(
        &self,
        token: &NamespaceToken,
        mut envelope: SenderEnvelope,
    ) -> RuntimeResult<SenderRecord> {
        envelope.namespace = token.namespace().as_str().to_owned();
        envelope.sender_assurance = SenderAssurance::Claimed;
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
    /// Record service admission time and schedule resubmission 600 seconds later.
    /// The timestamp is stored as Unix microseconds and survives restarts.
    pub async fn record_sender_transport_admission(
        &self,
        key: EnvelopeKey,
        admitted_at: chrono::DateTime<chrono::Utc>,
    ) -> RuntimeResult<()> {
        Ok(self
            .sender_transport_store()
            .record_admission(key, admitted_at.timestamp_micros())
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
    /// Set/release credit or policy holds, or set a key-change hold. A policy hold
    /// carries the evaluated mode and revision. Key-change holds can only be
    /// superseded through explicit confirmed re-encryption.
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
    use ring::signature::{Ed25519KeyPair, KeyPair};
    use uuid::Uuid;

    fn uuid(value: &str) -> Uuid {
        Uuid::parse_str(value).unwrap()
    }

    fn decode_hex(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    fn vector_receipt(disposition: ReceiptDisposition) -> DeliveryReceipt {
        let signature = match disposition {
            ReceiptDisposition::Stored => {
                "00de06d16479cd2a99fd6e66ae90ad66716c9af015fcba6b65db52029cbbe83bc9161dac398929cfb7955e3eb3cd8f4ae69e2cde7331d9aeb2a1f597a6e12f01"
            }
            ReceiptDisposition::Quarantined => {
                "1925fea98935bc24b684df54d625b60748de38292a55a6215802607e51b44fda8190120b7b5460562e1ab3404c7096931144ab6ac2d3201e1b5543b96939aa0c"
            }
        };
        DeliveryReceipt {
            binding: DeliveryReceiptBinding {
                protocol_version: 1,
                logical_message_id: uuid("6f1c2d3e-4a5b-4c6d-8e7f-90a1b2c3d4e5"),
                sender_agent_id: "01920000-0000-7000-8000-00000000a001".into(),
                recipient_agent_id: "01920000-0000-7000-8000-00000000a002".into(),
                recipient_device_id: uuid("01920000-0000-7000-8000-00000000d002"),
                recipient_key_epoch: 2,
                contact_generation: 3,
                delivery_attempt_id: uuid("01920000-0000-7000-8000-0000000e0001"),
            },
            disposition,
            signature: decode_hex(signature),
        }
    }

    fn vector_key(value: &str) -> [u8; 32] {
        decode_hex(value).try_into().unwrap()
    }

    fn signed_receipt(
        mut receipt: DeliveryReceipt,
        seed: &[u8; 32],
    ) -> (DeliveryReceipt, [u8; 32]) {
        let key_pair = Ed25519KeyPair::from_seed_unchecked(seed).unwrap();
        let input = receipt_signing_input(&receipt).unwrap();
        receipt.signature = key_pair.sign(&input).as_ref().to_vec();
        let pinned_key = key_pair.public_key().as_ref().try_into().unwrap();
        (receipt, pinned_key)
    }

    #[test]
    fn recipient_receipt_vectors_match_signing_input_and_verify() {
        let recipient_key =
            vector_key("a914d2b78bbef06e728db06ad577d1c09d04dae4a078ab7b7574187d9dc5d032");
        let vectors = [
            (
                vector_receipt(ReceiptDisposition::Stored),
                "6b686976652d6e6f64652d76312f7265636569707400000000016f1c2d3e4a5b4c6d8e7f90a1b2c3d4e50192000000007000800000000000a0010192000000007000800000000000a0020192000000007000800000000000d00200000000000000020000000000000003019200000000700080000000000e000101",
            ),
            (
                vector_receipt(ReceiptDisposition::Quarantined),
                "6b686976652d6e6f64652d76312f7265636569707400000000016f1c2d3e4a5b4c6d8e7f90a1b2c3d4e50192000000007000800000000000a0010192000000007000800000000000a0020192000000007000800000000000d00200000000000000020000000000000003019200000000700080000000000e000102",
            ),
        ];
        for (receipt, expected_input) in vectors {
            assert_eq!(
                receipt_signing_input(&receipt).unwrap(),
                decode_hex(expected_input)
            );
            VerifiedRecipientReceipt::verify(receipt, &recipient_key)
                .expect("recipient receipt vector must verify");
        }
    }

    #[test]
    fn invalid_recipient_receipt_signatures_are_refused() {
        let recipient_key =
            vector_key("a914d2b78bbef06e728db06ad577d1c09d04dae4a078ab7b7574187d9dc5d032");
        let sender_key =
            vector_key("9016672157bdb5b3529477312593f8e6fbf59641a52a374d50bd72fdf0f5d2af");
        let stored = vector_receipt(ReceiptDisposition::Stored);

        let mut quarantined_input = stored.clone();
        quarantined_input.disposition = ReceiptDisposition::Quarantined;
        assert!(VerifiedRecipientReceipt::verify(quarantined_input, &recipient_key).is_err());

        let mut different_attempt = stored.clone();
        different_attempt.binding.delivery_attempt_id =
            uuid("01920000-0000-7000-8000-0000000e0002");
        assert!(VerifiedRecipientReceipt::verify(different_attempt, &recipient_key).is_err());

        let mut different_message = stored.clone();
        different_message.binding.logical_message_id = uuid("6f1c2d3e-4a5b-4c6d-8e7f-90a1b2c3d4e6");
        assert!(VerifiedRecipientReceipt::verify(different_message, &recipient_key).is_err());

        let mut different_sender = stored.clone();
        different_sender.binding.sender_agent_id = "01920000-0000-7000-8000-00000000a003".into();
        assert!(VerifiedRecipientReceipt::verify(different_sender, &recipient_key).is_err());
        assert!(VerifiedRecipientReceipt::verify(stored.clone(), &sender_key).is_err());

        let mut invalid_agent_id = stored.clone();
        invalid_agent_id.binding.sender_agent_id = "not-a-uuid".into();
        assert!(VerifiedRecipientReceipt::verify(invalid_agent_id, &recipient_key).is_err());

        let mut wrong_signature_length = stored;
        wrong_signature_length.signature.pop();
        assert!(VerifiedRecipientReceipt::verify(wrong_signature_length, &recipient_key).is_err());
    }

    fn envelope() -> SenderEnvelope {
        SenderEnvelope {
            namespace: "untrusted-envelope-attribution".into(),
            logical_message_id: Uuid::new_v4(),
            outbound_note_id: Uuid::new_v4(),
            kind: "khive".into(),
            slug: "device".into(),
            credential_ref: "keys/device".into(),
            recipient_address: format!("khive1:example/{}", Uuid::nil()),
            protocol_version: 1,
            sender_agent_id: Uuid::new_v4().to_string(),
            sender_assurance: SenderAssurance::DaemonBearer,
            recipient_agent_id: Uuid::nil().to_string(),
            recipient_device_id: Uuid::new_v4(),
            recipient_key_epoch: 1,
            contact_generation: 1,
            sender_key_epoch: 1,
            recipient_key_fingerprint: "ab".repeat(32),
            enc: vec![1; 32],
            ciphertext: vec![2, 0, 255],
        }
    }

    #[tokio::test]
    async fn verified_receipt_uses_bound_backend_and_token_attribution() {
        let runtime = KhiveRuntime::memory().unwrap();
        let unrelated = KhiveRuntime::memory().unwrap();
        let token = NamespaceToken::local();
        let envelope = envelope();
        let key = envelope.key();
        let stored = runtime
            .create_sender_transport(&token, envelope.clone())
            .await
            .unwrap();
        assert_eq!(stored.envelope.namespace, token.namespace().as_str());
        assert_eq!(stored.envelope.sender_assurance, SenderAssurance::Claimed);
        assert_eq!(
            runtime
                .sender_transport(key)
                .await
                .unwrap()
                .unwrap()
                .envelope
                .sender_assurance,
            SenderAssurance::Claimed
        );
        assert!(
            unrelated.sender_transport(key).await.unwrap().is_none(),
            "transport must use bound backend"
        );
        let hold = HoldReason::PolicyDenied {
            mode: PolicyMode::Enforce,
            revision: 7,
        };
        let admitted_at = chrono::Utc::now();
        runtime
            .record_sender_transport_admission(key, admitted_at)
            .await
            .unwrap();
        let admitted = runtime.sender_transport(key).await.unwrap().unwrap();
        assert_eq!(admitted.admitted_at, Some(admitted_at.timestamp_micros()));
        assert_eq!(
            admitted.next_retry_at,
            Some(admitted_at.timestamp_micros() + 600_000_000)
        );
        runtime
            .hold_sender_transport(key, Some(hold))
            .await
            .unwrap();
        assert_eq!(
            runtime
                .sender_transport(key)
                .await
                .unwrap()
                .unwrap()
                .hold_reason,
            Some(hold)
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
        let (receipt, pinned_key) = signed_receipt(receipt, &[19; 32]);
        assert!(runtime
            .accept_verified_recipient_receipt(
                key,
                TransportState::RecipientQuarantined,
                VerifiedRecipientReceipt::verify(receipt.clone(), &pinned_key).unwrap()
            )
            .await
            .is_err());
        runtime
            .accept_verified_recipient_receipt(
                key,
                TransportState::RecipientStored,
                VerifiedRecipientReceipt::verify(receipt, &pinned_key).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            runtime
                .sender_transport(key)
                .await
                .unwrap()
                .unwrap()
                .hold_reason,
            None
        );
        assert_eq!(
            runtime.sender_transport(key).await.unwrap().unwrap().state,
            TransportState::RecipientStored
        );
    }
    #[tokio::test]
    async fn caller_sender_assurance_is_claimed() {
        for assurance in [
            SenderAssurance::DaemonBearer,
            SenderAssurance::ActorSignature,
        ] {
            let runtime = KhiveRuntime::memory().unwrap();
            let token = NamespaceToken::local();
            let mut envelope = envelope();
            envelope.sender_assurance = assurance;
            let first = runtime
                .create_sender_transport(&token, envelope.clone())
                .await
                .unwrap();
            assert_eq!(
                first.envelope.sender_assurance,
                SenderAssurance::Claimed,
                "create must normalize caller assurance"
            );
            assert_eq!(
                runtime.sender_transport(envelope.key()).await.unwrap(),
                Some(first)
            );
            runtime
                .hold_sender_transport(envelope.key(), Some(HoldReason::RecipientKeyChanged))
                .await
                .unwrap();
            envelope.recipient_key_epoch += 1;
            let next = runtime
                .reencrypt_sender_transport_after_confirmed_key_change(&token, envelope.clone())
                .await
                .expect("re-encryption must normalize caller assurance");
            assert_eq!(next.envelope.sender_assurance, SenderAssurance::Claimed);
            assert_eq!(
                runtime.sender_transport(envelope.key()).await.unwrap(),
                Some(next)
            );
        }
    }
}
