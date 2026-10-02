//! Trusted in-process recipient ingest. No registry verb dispatches this API.
//!
//! The authenticated node-loop issuer is not yet present, so this entry point
//! is unavailable to callers outside the runtime crate.
//!
//! ```compile_fail
//! use khive_runtime::KhiveRuntime;
//! fn main() { let _ = KhiveRuntime::ingest_verified_recipient; }
//! ```
//!
//! ```compile_fail
//! use khive_runtime::comm_recipient::LocalRecipientBinding;
//! fn main() { let _ = std::mem::size_of::<LocalRecipientBinding>(); }
//! ```
//!
//! ```compile_fail
//! use khive_runtime::comm_recipient::VerifiedInboundContent;
//! fn main() { let _ = std::mem::size_of::<VerifiedInboundContent>(); }
//! ```
#![allow(dead_code)] // The verified node-loop caller is supplied by #3537.
use crate::{KhiveRuntime, NamespaceToken, RuntimeError, RuntimeResult};
use khive_channel::InboundReceiptTicket;
pub use khive_db::stores::note::recipient::{
    QuarantineReason, RecipientCommitResult, RecipientDisposition,
};
use khive_db::stores::note::recipient::{
    QuarantineRecord, RecipientCommit, RecipientTransportStore,
};
use khive_storage::Note;
use serde_json::json;
use uuid::Uuid;

/// Local enrollment authority, supplied by the trusted slug owner. Never fill
/// this from plaintext or a wire request. Actor labels are local, not agent UUIDs.
pub(crate) struct LocalRecipientBinding {
    pub actor: String,
    pub realm: String,
    pub slug: String,
    pub agent_id: String,
    pub device_id: Uuid,
    pub key_epoch: u64,
}
/// The only purpose values a protocol-v1 sender may declare.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeclaredMessageKind {
    Announce,
    Report,
    Ask,
}
impl DeclaredMessageKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Announce => "announce",
            Self::Report => "report",
            Self::Ask => "ask",
        }
    }
}

/// Content has deliberately no sender, recipient, namespace or actor field.
/// Identity comes exclusively from local binding and an authenticated ticket.
pub(crate) enum VerifiedInboundContent {
    Message {
        content: String,
        subject: Option<String>,
        kind: Option<DeclaredMessageKind>,
        in_reply_to: Option<Uuid>,
        correlation: Option<String>,
        sent_at: String,
    },
    Quarantine {
        reason: QuarantineReason,
        /// The parsed plaintext object, present exactly when the pair policy
        /// refused a valid message (ADR-105 A.8 Receiving, step 4).
        parsed_plaintext: Option<serde_json::Value>,
    },
}
fn invalid(message: &str) -> RuntimeError {
    RuntimeError::InvalidInput(message.into())
}
fn canonical_id(id: &str) -> bool {
    Uuid::parse_str(id)
        .ok()
        .is_some_and(|u| u.to_string() == id)
}
impl KhiveRuntime {
    /// Commit an authenticated delivery on comm's assigned runtime. The caller
    /// MUST authenticate/decrypt and verify enrollment before creating the ticket.
    /// This method performs no cryptography; it consumes the in-memory ticket,
    /// checks local binding, and commits note/replay/ack/quarantine atomically.
    ///
    /// For quarantine, delivery_item is the exact original JSON object, bounded
    /// at 98,304 bytes by the protocol request limit. No blob store is involved.
    /// A failure commits nothing and returns no recipient disposition. Retryable
    /// storage errors must not be converted into quarantine by the caller.
    /// A SecretDetected refusal is deterministic: redelivering the same payload
    /// is refused again.
    pub(crate) async fn ingest_verified_recipient(
        &self,
        token: &NamespaceToken,
        local: &LocalRecipientBinding,
        ticket: InboundReceiptTicket,
        payload: VerifiedInboundContent,
        delivery_item: Vec<u8>,
    ) -> RuntimeResult<RecipientCommitResult> {
        let binding = ticket.binding();
        if !canonical_id(&binding.sender_agent_id)
            || !canonical_id(&binding.recipient_agent_id)
            || !canonical_id(&local.agent_id)
        {
            return Err(invalid("verified receipt agents must be canonical UUIDs"));
        }
        if binding.protocol_version != 1
            || binding.recipient_agent_id != local.agent_id
            || binding.recipient_device_id != local.device_id
            || binding.recipient_key_epoch != local.key_epoch
        {
            return Err(invalid(
                "verified delivery does not match local recipient binding",
            ));
        }
        if [
            binding.recipient_key_epoch,
            binding.contact_generation,
            ticket.sender_key_epoch(),
        ]
        .iter()
        .any(|n| *n == 0 || *n > u32::MAX as u64)
        {
            return Err(invalid("invalid ticket epoch or generation"));
        }
        if local.actor.trim().is_empty()
            || local.slug.is_empty()
            || local.realm.is_empty()
            || local.realm.len() > 64
            || !local
                .realm
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
        {
            return Err(invalid("invalid local recipient route"));
        }
        // ADR-105 A.8 step 4: a successfully opened replay is answered from
        // its durable claim before interpreting the new attempt's plaintext.
        let binding_value = serde_json::to_value(binding)
            .map_err(|error| invalid(&format!("invalid receipt binding: {error}")))?;
        let recipient_store = RecipientTransportStore::new(self.backend().pool_arc());
        if let Some(replay) = recipient_store
            .ack_if_replayed(binding_value.clone(), &local.actor)
            .await?
        {
            return Ok(replay);
        }
        self.validate_note_kind("message")?;
        let from = format!("khive1:{}/{}", local.realm, binding.sender_agent_id);
        let received_at = chrono::Utc::now().to_rfc3339();
        let mut in_reply_to = None;
        let mut correlation = None;
        let (note, disposition, quarantine) = match payload {
            VerifiedInboundContent::Message {
                content,
                subject,
                kind,
                in_reply_to: parent,
                correlation: message_correlation,
                sent_at,
            } => {
                let parsed_sent_at = chrono::DateTime::parse_from_rfc3339(&sent_at)
                    .ok()
                    .filter(|stamp| stamp.offset().local_minus_utc() == 0);
                if let Some(stamp) = parsed_sent_at {
                    // An empty body is a valid A.5 message and is stored like any other.
                    crate::secret_gate::check_at(&content, "note", "content")?;
                    if let Some(subject) = &subject {
                        crate::secret_gate::check_at(subject, "note", "name")?;
                    }
                    in_reply_to = parent;
                    correlation = message_correlation;
                    let mut note = Note::new(token.namespace().as_str(), "message", content);
                    note.name = subject.clone();
                    let mut props = json!({
                        "comm_schema_version": 1,
                        "from": from,
                        "from_actor": from,
                        "to": local.actor,
                        "to_actor": local.actor,
                        "direction": "inbound",
                        "read": false,
                        "received_at": received_at,
                        "channel_kind": "khive",
                        "channel_slug": local.slug,
                        "logical_message_id": binding.logical_message_id,
                        "sent_at": stamp
                            .with_timezone(&chrono::Utc)
                            .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
                        "message_kind": kind.map_or("unspecified", DeclaredMessageKind::as_str),
                    });
                    if let Some(subject) = subject {
                        props["subject"] = json!(subject);
                    }
                    if let Some(parent) = in_reply_to {
                        props["in_reply_to"] = json!(parent);
                    }
                    crate::secret_gate::check_json_at(&props, "note", "properties")?;
                    note.properties = Some(props);
                    (Some(note), RecipientDisposition::Stored, None)
                } else {
                    (
                        None,
                        RecipientDisposition::Quarantined,
                        Some(QuarantineRecord {
                            reason: QuarantineReason::InvalidPlaintext,
                            delivery_item,
                            parsed_plaintext: None,
                        }),
                    )
                }
            }
            VerifiedInboundContent::Quarantine {
                reason,
                parsed_plaintext,
            } => {
                // Retained plaintext passes the same write-time secret gate as a stored message.
                // Refuse before the quarantine/replay/ack transaction can write.
                if let Some(plaintext) = &parsed_plaintext {
                    crate::secret_gate::check_json_at(plaintext, "quarantine", "parsed_plaintext")?;
                }
                (
                    None,
                    RecipientDisposition::Quarantined,
                    Some(QuarantineRecord {
                        reason,
                        delivery_item,
                        parsed_plaintext,
                    }),
                )
            }
        };
        let result = recipient_store
            .commit(RecipientCommit {
                note,
                recipient_actor: local.actor.clone(),
                binding: binding_value,
                sender_agent_id: binding.sender_agent_id.clone(),
                logical_message_id: binding.logical_message_id,
                delivery_attempt_id: binding.delivery_attempt_id,
                disposition,
                quarantine,
                in_reply_to,
                correlation,
            })
            .await?;
        if let Some(note) = &result.note {
            // Like ordinary ingest, indexing is best-effort after the durable commit.
            if let Ok(fts) = self.text_for_notes(token) {
                if let Err(error) = fts
                    .upsert_document(crate::curation::note_fts_document(note))
                    .await
                {
                    tracing::warn!(note_id=%note.id,error=%error,"verified ingest FTS indexing failed");
                }
            }
            for model in self.registered_embedding_model_names() {
                match self
                    .embed_document_with_model_outcome_for_token(
                        token,
                        &model,
                        crate::curation::note_embedding_text_ref(note),
                    )
                    .await
                {
                    Ok(outcome) => {
                        if let Err(error) = self
                            .publish_note_vector_revision(token, note, &model, &outcome.vector)
                            .await
                        {
                            tracing::warn!(note_id=%note.id,error=%error,"verified ingest vector indexing failed");
                        }
                    }
                    Err(error) => {
                        tracing::warn!(note_id=%note.id,error=%error,"verified ingest embedding failed")
                    }
                }
            }
        }
        Ok(result)
    }
}
#[cfg(test)]
mod tests;
