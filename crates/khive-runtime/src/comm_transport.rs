//! Runtime-owned sender transport persistence (ADR-105).
//!
//! Call these methods on the runtime bound to comm's assigned backend, exactly
//! as for ordinary comm note operations. Routing happens before this boundary;
//! the adapter never opens a database or supplies SQL. Receipt signatures are
//! checked against the owner's pinned recipient key before durable state changes.
use crate::error::RuntimeResult;
use crate::{KhiveRuntime, NamespaceToken};
use khive_channel::VerifiedRecipientReceipt;
use khive_db::stores::note::transport::SenderTransportStore;
pub use khive_db::stores::note::transport::{
    EnvelopeKey, FailureClass, HoldReason, PolicyMode, SenderAssurance, SenderEnvelope,
    SenderRecord, TransportState,
};

/// Sender-local delivery status, including the absence of a transport record.
/// Holds retain `Pending`; `Unknown` is never persisted as a transport state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportStatus {
    Pending,
    RecipientStored,
    RecipientQuarantined,
    Failed,
    Unknown,
}

impl KhiveRuntime {
    fn sender_transport_store(&self) -> SenderTransportStore {
        SenderTransportStore::new(self.backend().pool_arc())
    }
    /// Read transport status in the token's primary namespace on the bound backend.
    /// This does not infer the internal inbound sibling or mutate retry metadata.
    pub async fn sender_transport_status(
        &self,
        token: &NamespaceToken,
        outbound_note_id: uuid::Uuid,
    ) -> RuntimeResult<TransportStatus> {
        let Some(row) = self
            .sender_transport_store()
            .get_by_outbound_note_id(token.namespace().as_str(), outbound_note_id)
            .await?
        else {
            return Ok(TransportStatus::Unknown);
        };
        if row.hold_reason.is_some() {
            return Ok(TransportStatus::Pending);
        }
        Ok(match row.state {
            TransportState::Pending => TransportStatus::Pending,
            TransportState::RecipientStored => TransportStatus::RecipientStored,
            TransportState::RecipientQuarantined => TransportStatus::RecipientQuarantined,
            TransportState::Failed => TransportStatus::Failed,
        })
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
    ///
    /// An unverified payload cannot cross this boundary:
    ///
    /// ```compile_fail
    /// use khive_channel::DeliveryReceipt;
    /// use khive_runtime::{KhiveRuntime, comm_transport::{EnvelopeKey, TransportState}};
    /// async fn accept(runtime: &KhiveRuntime, key: EnvelopeKey, receipt: DeliveryReceipt) {
    ///     runtime.accept_verified_recipient_receipt(key, TransportState::RecipientStored, receipt)
    ///         .await.unwrap();
    /// }
    /// ```
    ///
    /// Its positive twin uses the channel constructor and the same runtime method:
    ///
    /// ```
    /// use khive_channel::{DeliveryReceipt, VerifiedRecipientReceipt};
    /// use khive_runtime::{KhiveRuntime, comm_transport::{EnvelopeKey, TransportState}};
    /// async fn accept(runtime: &KhiveRuntime, key: EnvelopeKey, receipt: DeliveryReceipt,
    ///     pinned_key: &[u8; 32]) {
    ///     let verified = VerifiedRecipientReceipt::verify(receipt, pinned_key).unwrap();
    ///     runtime.accept_verified_recipient_receipt(key, TransportState::RecipientStored, verified)
    ///         .await.unwrap();
    /// }
    /// ```
    pub async fn accept_verified_recipient_receipt(
        &self,
        key: EnvelopeKey,
        state: TransportState,
        receipt: VerifiedRecipientReceipt,
    ) -> RuntimeResult<()> {
        let receipt = serde_json::to_value(receipt.receipt()).map_err(|error| {
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
    use khive_channel::{
        receipt_signing_input, DeliveryReceipt, DeliveryReceiptBinding, ReceiptDisposition,
    };
    use ring::signature::{Ed25519KeyPair, KeyPair};
    use uuid::Uuid;

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

    async fn accept_status_receipt(
        runtime: &KhiveRuntime,
        envelope: &SenderEnvelope,
        disposition: ReceiptDisposition,
    ) {
        let state = match disposition {
            ReceiptDisposition::Stored => TransportState::RecipientStored,
            ReceiptDisposition::Quarantined => TransportState::RecipientQuarantined,
        };
        let receipt = DeliveryReceipt {
            binding: DeliveryReceiptBinding {
                protocol_version: envelope.protocol_version,
                logical_message_id: envelope.logical_message_id,
                sender_agent_id: envelope.sender_agent_id.clone(),
                recipient_agent_id: envelope.recipient_agent_id.clone(),
                recipient_device_id: envelope.recipient_device_id,
                recipient_key_epoch: envelope.recipient_key_epoch,
                contact_generation: envelope.contact_generation,
                delivery_attempt_id: Uuid::new_v4(),
            },
            disposition,
            signature: vec![],
        };
        let (receipt, pinned_key) = signed_receipt(receipt, &[23; 32]);
        runtime
            .accept_verified_recipient_receipt(
                envelope.key(),
                state,
                VerifiedRecipientReceipt::verify(receipt, &pinned_key).unwrap(),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn transport_status_pending_admission_and_hold() {
        let runtime = KhiveRuntime::memory().unwrap();
        let token = NamespaceToken::local();
        let e = envelope();
        runtime
            .create_sender_transport(&token, e.clone())
            .await
            .unwrap();
        assert_eq!(
            runtime
                .sender_transport_status(&token, e.outbound_note_id)
                .await
                .unwrap(),
            TransportStatus::Pending,
            "a newly persisted envelope is pending"
        );
        runtime
            .record_sender_transport_admission(e.key(), chrono::Utc::now())
            .await
            .unwrap();
        assert_eq!(
            runtime
                .sender_transport_status(&token, e.outbound_note_id)
                .await
                .unwrap(),
            TransportStatus::Pending,
            "service admission is not recipient storage"
        );
        runtime
            .hold_sender_transport(e.key(), Some(HoldReason::InsufficientCredit))
            .await
            .unwrap();
        assert_eq!(
            runtime
                .sender_transport_status(&token, e.outbound_note_id)
                .await
                .unwrap(),
            TransportStatus::Pending,
            "an insufficient-credit hold is pending, never a further status"
        );
    }

    #[tokio::test]
    async fn transport_status_failed_and_verified_receipts() {
        let runtime = KhiveRuntime::memory().unwrap();
        let token = NamespaceToken::local();
        for (disposition, expected) in [
            (ReceiptDisposition::Stored, TransportStatus::RecipientStored),
            (
                ReceiptDisposition::Quarantined,
                TransportStatus::RecipientQuarantined,
            ),
        ] {
            let e = envelope();
            runtime
                .create_sender_transport(&token, e.clone())
                .await
                .unwrap();
            runtime
                .record_sender_transport_failure(e.key(), FailureClass::Permanent, None)
                .await
                .unwrap();
            assert_eq!(
                runtime
                    .sender_transport_status(&token, e.outbound_note_id)
                    .await
                    .unwrap(),
                TransportStatus::Failed,
                "a permanent local refusal is failed"
            );
            accept_status_receipt(&runtime, &e, disposition).await;
            assert_eq!(
                runtime
                    .sender_transport_status(&token, e.outbound_note_id)
                    .await
                    .unwrap(),
                expected,
                "a verified receipt supersedes the local failure"
            );
        }
        assert_eq!(
            runtime
                .sender_transport_status(&token, Uuid::new_v4())
                .await
                .unwrap(),
            TransportStatus::Unknown,
            "an absent outbound UUID is unknown"
        );
    }

    #[tokio::test]
    async fn transport_status_is_primary_namespace_and_backend_scoped() {
        let runtime = KhiveRuntime::memory().unwrap();
        let token_a = NamespaceToken::for_namespace(crate::Namespace::parse("sender-a").unwrap());
        let token_b = NamespaceToken::mint_with_visibility(
            crate::Namespace::parse("sender-b").unwrap(),
            vec![crate::Namespace::parse("sender-a").unwrap()],
            crate::ActorRef::anonymous(),
        );
        let e = envelope();
        runtime
            .create_sender_transport(&token_a, e.clone())
            .await
            .unwrap();
        assert_eq!(
            runtime
                .sender_transport(e.key())
                .await
                .unwrap()
                .unwrap()
                .envelope
                .namespace,
            "sender-a"
        );
        assert_eq!(
            runtime
                .sender_transport_status(&token_a, e.outbound_note_id)
                .await
                .unwrap(),
            TransportStatus::Pending
        );
        assert_eq!(
            runtime
                .sender_transport_status(&token_b, e.outbound_note_id)
                .await
                .unwrap(),
            TransportStatus::Unknown,
            "additional visible namespaces cannot reveal transport status"
        );
        let unrelated = KhiveRuntime::memory().unwrap();
        assert_eq!(
            unrelated
                .sender_transport_status(&token_a, e.outbound_note_id)
                .await
                .unwrap(),
            TransportStatus::Unknown,
            "a different bound backend has no record"
        );
    }

    #[tokio::test]
    async fn transport_status_read_preserves_every_sender_field() {
        let runtime = KhiveRuntime::memory().unwrap();
        let token = NamespaceToken::local();
        let e = envelope();
        runtime
            .create_sender_transport(&token, e.clone())
            .await
            .unwrap();
        let before = runtime.sender_transport(e.key()).await.unwrap().unwrap();
        assert_eq!(before.admitted_at, None);
        for _ in 0..3 {
            assert_eq!(
                runtime
                    .sender_transport_status(&token, e.outbound_note_id)
                    .await
                    .unwrap(),
                TransportStatus::Pending
            );
        }
        let after = runtime.sender_transport(e.key()).await.unwrap().unwrap();
        assert_eq!(
            after, before,
            "a status read preserves envelope bytes, holds, attempts and timestamps"
        );
    }
}
