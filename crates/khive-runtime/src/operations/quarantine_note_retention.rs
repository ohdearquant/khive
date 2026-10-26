use std::collections::BTreeMap;

use khive_storage::error::{StorageError, WriterTaskRequestState};
use khive_storage::note::{NotePropertyPatch, NotePropertyPrecondition};
use khive_storage::types::SqlValue;
use khive_storage::{
    Attachment, AttachmentStore, AttachmentSubstrate, ContentRef, NewAttachment, Note,
    StorageCapability,
};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{KhiveRuntime, NamespaceToken, RuntimeError, RuntimeResult};

/// Original-byte custody chosen before a trusted message is published.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuarantineOriginalRetention {
    Retained,
    BlobSweepActive,
    OwnerMismatch,
}

impl QuarantineOriginalRetention {
    pub fn not_retained_reason(self) -> Option<&'static str> {
        match self {
            Self::Retained => None,
            Self::BlobSweepActive => Some("blob-sweep-active"),
            Self::OwnerMismatch => Some("owner-mismatch"),
        }
    }
}

/// Preserve retention facts even when post-commit derivation returns an error.
/// A failed or unknown write is not a successful custody receipt.
pub struct TrustedIngestAttachmentOutcome {
    pub result: RuntimeResult<Option<Note>>,
    pub retention: QuarantineOriginalRetention,
}

impl KhiveRuntime {
    /// Like [`Self::try_create_note`] but permits the caller to establish the
    /// transport-owned `message` properties that `try_create_note` refuses.
    ///
    /// This is a deliberately named, separate entry point rather than a flag
    /// on `try_create_note` so the trust decision is visible at every call
    /// site: `comm.ingest` (`khive-pack-comm/src/handlers/ingest.rs`) is the sole
    /// legitimate caller, because it is the only code that has just derived
    /// quarantine disposition and channel provenance from the inbound
    /// transport itself. The caller set is bounded by possession, not
    /// documentation: the required [`crate::ChannelIngestCapability`] is
    /// constructible only inside this crate and granted at pack registration
    /// exclusively to channel-transport packs. Every other write path uses
    /// `try_create_note`, which rejects those properties
    /// unconditionally.
    #[allow(clippy::too_many_arguments)]
    pub async fn try_create_note_as_trusted_ingest(
        &self,
        _capability: &crate::pack::ChannelIngestCapability,
        token: &NamespaceToken,
        kind: &str,
        name: Option<&str>,
        content: &str,
        properties: Option<serde_json::Value>,
        expires_after: Option<std::time::Duration>,
    ) -> RuntimeResult<Option<Note>> {
        self.try_create_note_impl(
            token,
            kind,
            name,
            content,
            properties,
            true,
            None,
            expires_after,
            None,
        )
        .await
    }

    /// Publish a trusted message and its original. Main-backend writes remain
    /// atomic. A secondary quarantine registers its canonical main owner before
    /// publishing the reference, under the existing blob-sweep SQL fence.
    #[allow(clippy::too_many_arguments)]
    pub async fn try_create_note_as_trusted_ingest_with_attachment(
        &self,
        capability: &crate::pack::ChannelIngestCapability,
        token: &NamespaceToken,
        kind: &str,
        name: Option<&str>,
        content: &str,
        properties: Option<serde_json::Value>,
        attachment: NewAttachment,
        expires_after: Option<std::time::Duration>,
    ) -> RuntimeResult<Option<Note>> {
        self.try_create_note_as_trusted_ingest_with_attachment_report(
            capability,
            token,
            kind,
            name,
            content,
            properties,
            attachment,
            expires_after,
        )
        .await
        .result
    }

    /// Like the attachment entry point, retaining the exact custody outcome
    /// alongside the original result, including a committed degradation error.
    #[allow(clippy::too_many_arguments)]
    pub async fn try_create_note_as_trusted_ingest_with_attachment_report(
        &self,
        _capability: &crate::pack::ChannelIngestCapability,
        token: &NamespaceToken,
        kind: &str,
        name: Option<&str>,
        content: &str,
        properties: Option<serde_json::Value>,
        attachment: NewAttachment,
        expires_after: Option<std::time::Duration>,
    ) -> TrustedIngestAttachmentOutcome {
        let mut retention = QuarantineOriginalRetention::Retained;
        let result = self
            .try_create_note_impl(
                token,
                kind,
                name,
                content,
                properties,
                true,
                Some(attachment),
                expires_after,
                Some(&mut retention),
            )
            .await;
        TrustedIngestAttachmentOutcome { result, retention }
    }

    #[cfg(any(test, feature = "test-internals"))]
    #[doc(hidden)]
    pub async fn test_insert_secondary_quarantine_note(
        &self,
        token: &NamespaceToken,
        note: &mut Note,
        attachment: NewAttachment,
        retention: &mut QuarantineOriginalRetention,
    ) -> RuntimeResult<bool> {
        self.try_insert_secondary_quarantine_note(token, note, attachment, retention)
            .await
    }

    pub(super) async fn try_insert_secondary_quarantine_note(
        &self,
        token: &NamespaceToken,
        note: &mut Note,
        attachment: NewAttachment,
        retention: &mut QuarantineOriginalRetention,
    ) -> RuntimeResult<bool> {
        // Resolve the secondary note store before creating a main owner. No note
        // publication has been attempted if any setup or blob check fails.
        let notes = self.raw_notes(token)?;
        let core = self.core();
        let owners = core.attachments()?;
        let blob = core.blob_store().ok_or_else(|| {
            RuntimeError::Unconfigured(
                "trusted ingest attachment requires an installed BlobStore".into(),
            )
        })?;
        let created = match owners
            .try_insert_attachment(Attachment::from_new(
                note.id,
                AttachmentSubstrate::Note,
                attachment.clone(),
                note.created_at,
            ))
            .await
        {
            Ok(true) => true,
            Ok(false) => {
                let existing = owners.get_attachment(note.id, &attachment.role).await?;
                if !matches!(existing, Some(ref owner)
                    if owner.substrate == AttachmentSubstrate::Note
                        && owner.content_ref == attachment.content_ref)
                {
                    *retention = QuarantineOriginalRetention::OwnerMismatch;
                }
                false
            }
            Err(error) if owner_insert_was_fenced(&error) => {
                *retention = QuarantineOriginalRetention::BlobSweepActive;
                false
            }
            Err(error) => return Err(error.into()),
        };
        if *retention == QuarantineOriginalRetention::Retained {
            // Registration wins before this check, preventing a new GC claim.
            // A completed earlier sweep may already have removed an old ref.
            let published = match blob.exists(&attachment.content_ref).await {
                Ok(true) => Ok(()),
                Ok(false) => Err(RuntimeError::InvalidInput(format!(
                    "trusted ingest attachment refers to an unpublished blob: {}",
                    attachment.content_ref,
                ))),
                Err(error) => Err(error.into()),
            };
            if let Err(error) = published {
                if created {
                    release_new_owner(owners.as_ref(), note, &attachment).await?;
                }
                return Err(error);
            }
        }
        let properties = note
            .properties
            .as_mut()
            .and_then(Value::as_object_mut)
            .ok_or_else(|| {
                RuntimeError::Internal("secondary quarantine is missing its metadata object".into())
            })?;
        if let Some(reason) = retention.not_retained_reason() {
            properties.remove("quarantine_content_ref");
            properties.insert("quarantine_original_retained".into(), json!("false"));
            properties.insert(
                "quarantine_original_not_retained_reason".into(),
                json!(reason),
            );
        } else {
            properties.insert(
                "quarantine_content_ref".into(),
                json!(attachment.content_ref.to_string()),
            );
            properties.insert("quarantine_original_retained".into(), json!("true"));
            properties.remove("quarantine_original_not_retained_reason");
        }
        let inserted = notes.try_insert_note(note.clone()).await;
        let known_noncommit = match &inserted {
            Ok(false) => true,
            Err(error) => secondary_write_did_not_commit(error),
            Ok(true) => false,
        };
        if created && known_noncommit {
            release_new_owner(owners.as_ref(), note, &attachment).await?;
        }
        Ok(inserted?)
    }

    /// Repair the original-byte reference and retention of a matching quarantined message.
    /// The capability permits only this fixed property set; storage rechecks the
    /// channel and original-byte identity against the current row before writing.
    #[allow(clippy::too_many_arguments)]
    pub async fn try_repair_quarantined_note_retention(
        &self,
        _capability: &crate::ChannelIngestCapability,
        token: &NamespaceToken,
        id: Uuid,
        channel_kind: &str,
        channel_slug: &str,
        content_ref: &ContentRef,
        extend_expires_at: Option<i64>,
        updated_at: i64,
    ) -> RuntimeResult<bool> {
        let set = BTreeMap::from([
            ("channel_slug".into(), Value::String(channel_slug.into())),
            (
                "quarantine_content_ref".into(),
                Value::String(content_ref.to_string()),
            ),
        ]);
        crate::secret_gate::reject_reserved_secret_gate_property(Some(&json!(&set)))?;
        let patch = NotePropertyPatch {
            preconditions: vec![
                NotePropertyPrecondition::AbsentOrExtractEquals {
                    key: "quarantine_content_ref".into(),
                    value: SqlValue::Text(content_ref.to_string()),
                },
                NotePropertyPrecondition::ExtractEquals {
                    key: "channel_kind".into(),
                    value: SqlValue::Text(channel_kind.into()),
                },
                NotePropertyPrecondition::AbsentOrTextEquals {
                    key: "channel_slug".into(),
                    value: channel_slug.into(),
                },
                NotePropertyPrecondition::TrueOrTextTrue {
                    key: "quarantined".into(),
                },
            ],
            set,
            extend_expires_at,
            updated_at,
        };
        Ok(self
            .raw_notes(token)?
            .try_patch_note_properties(id, token.namespace().as_str(), "message", &patch)
            .await?)
    }
}

// This exact native trigger message is the canonical attachment fence in
// khive-db/sql/047-attachment-role-quarantine.sql. Generic storage failures
// must not be misreported as a sweep or converted to successful ingestion.
fn owner_insert_was_fenced(error: &StorageError) -> bool {
    match error {
        StorageError::SqliteWrite { failure, source } if !failure.settlement_unknown => {
            owner_insert_was_fenced(source)
        }
        StorageError::WriterTaskRequestFailed {
            request_state,
            source,
        } if matches!(
            request_state,
            WriterTaskRequestState::NotStarted | WriterTaskRequestState::TransactionRolledBack
        ) =>
        {
            owner_insert_was_fenced(source)
        }
        StorageError::Driver {
            capability,
            operation,
            source,
        } if *capability == StorageCapability::Attachments
            && operation.as_ref() == "try_insert_attachment" =>
        {
            source
                .to_string()
                .contains("content_ref is reserved by an active blob sweep")
        }
        _ => false,
    }
}

fn secondary_write_did_not_commit(error: &StorageError) -> bool {
    match error {
        StorageError::SqliteWrite { failure, source } if !failure.settlement_unknown => {
            secondary_write_did_not_commit(source)
        }
        StorageError::WriterTaskRequestFailed { request_state, .. }
        | StorageError::WriterTaskTerminated { request_state, .. } => matches!(
            request_state,
            WriterTaskRequestState::NotStarted | WriterTaskRequestState::TransactionRolledBack,
        ),
        StorageError::AdmissionTimeout { .. }
        | StorageError::WriteQueueFull { .. }
        | StorageError::WriterTaskBusy { .. }
        | StorageError::WriterTaskNoRuntime => true,
        // A statement/driver/timeout error alone does not establish finality.
        _ => false,
    }
}

async fn release_new_owner(
    owners: &dyn AttachmentStore,
    note: &Note,
    attachment: &NewAttachment,
) -> RuntimeResult<()> {
    owners.delete_attachment_if(
        note.id, &attachment.role, AttachmentSubstrate::Note, &attachment.content_ref,
    ).await.map_err(|error| RuntimeError::Internal(format!(
        "secondary quarantine note {} did not publish, but its original owner could not be released: {error}",
        note.id,
    )))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BackendId, RuntimeConfig, StorageBackend};
    use std::sync::Arc;

    #[tokio::test]
    async fn trusted_retention_repair_uses_current_backend_and_preserves_unrelated_properties() {
        let main = Arc::new(StorageBackend::memory().unwrap());
        let channel = Arc::new(StorageBackend::memory().unwrap());
        main.prepare_core_schema().unwrap();
        channel.prepare_core_schema().unwrap();
        let mut config = RuntimeConfig::no_embeddings();
        config.backend_id = BackendId::parse("channel").unwrap();
        let runtime = KhiveRuntime::from_backend(channel, config).with_core_backend(main);
        let token = NamespaceToken::local();
        let capability = crate::ChannelIngestCapability::grant_for_direct_composition();
        let content_ref = ContentRef::from_hex("a".repeat(64)).unwrap();
        let mut note = Note::new("local", "message", "quarantine").with_properties(json!({
            "channel_kind": "email",
            "quarantined": "true",
            "unrelated": {"keep": [1, true, null]},
            "khive:secret_gate": {"preserve": true},
        }));
        note.updated_at = 100;
        note.expires_at = Some(400);
        let raw = runtime.raw_notes(&token).unwrap();
        raw.upsert_note(note.clone()).await.unwrap();
        let main_notes = runtime.core().raw_notes(&token).unwrap();
        main_notes.upsert_note(note.clone()).await.unwrap();
        let main_before = main_notes.get_note(note.id).await.unwrap();

        assert!(runtime
            .try_repair_quarantined_note_retention(
                &capability,
                &token,
                note.id,
                "email",
                "mailbox",
                &content_ref,
                Some(300),
                90,
            )
            .await
            .unwrap());
        let repaired = raw.get_note(note.id).await.unwrap().unwrap();
        assert_eq!(repaired.updated_at, 100);
        assert_eq!(repaired.expires_at, Some(400));
        assert_eq!(
            repaired.properties.as_ref().unwrap()["channel_slug"],
            "mailbox"
        );
        assert_eq!(
            repaired.properties.as_ref().unwrap()["quarantine_content_ref"],
            content_ref.to_string()
        );
        for key in ["unrelated", "khive:secret_gate"] {
            assert_eq!(
                repaired.properties.as_ref().unwrap()[key],
                note.properties.as_ref().unwrap()[key]
            );
        }
        assert_eq!(main_notes.get_note(note.id).await.unwrap(), main_before);

        assert!(!runtime
            .try_repair_quarantined_note_retention(
                &capability,
                &token,
                note.id,
                "email",
                "other-mailbox",
                &content_ref,
                Some(500),
                200,
            )
            .await
            .unwrap());
        assert_eq!(raw.get_note(note.id).await.unwrap().unwrap(), repaired);
    }

    #[test]
    fn secondary_write_finality_accepts_proven_noncommit() {
        use khive_storage::error::{StorageError, WriterTaskRequestState};

        for state in [
            WriterTaskRequestState::NotStarted,
            WriterTaskRequestState::TransactionRolledBack,
        ] {
            let error = StorageError::writer_task_terminated(state);
            assert!(
                super::secondary_write_did_not_commit(&error),
                "explicitly settled noncommit must permit cleanup: {error}"
            );
        }
        let rolled_back = StorageError::WriterTaskRequestFailed {
            request_state: WriterTaskRequestState::TransactionRolledBack,
            source: Box::new(StorageError::Internal("controlled rollback".into())),
        };
        assert!(super::secondary_write_did_not_commit(&rolled_back));
    }

    #[test]
    fn secondary_write_finality_preserves_outer_unknown_and_ambiguity() {
        use khive_storage::error::{
            SqliteWriteFailure, SqliteWriteStage, StorageError, WriterTaskRequestState,
        };

        let outer_unknown =
            StorageError::writer_task_terminated(WriterTaskRequestState::TransactionRolledBack)
                .with_sqlite_write_failure(SqliteWriteFailure {
                    stage: SqliteWriteStage::Commit,
                    primary_code: 13,
                    extended_code: 13,
                    settlement_unknown: true,
                });
        assert!(
            !super::secondary_write_did_not_commit(&outer_unknown),
            "outer settlement uncertainty must preserve the owner despite nested known state"
        );
        for error in [
            StorageError::writer_task_terminated(WriterTaskRequestState::SideEffectsUnknown),
            StorageError::Timeout {
                operation: "try_insert_note".into(),
            },
            StorageError::Transaction {
                operation: "try_insert_note".into(),
                message: "outcome not established".into(),
            },
            StorageError::driver(
                khive_storage::StorageCapability::Notes,
                "try_insert_note",
                std::io::Error::other("outcome not established"),
            ),
        ] {
            assert!(
                !super::secondary_write_did_not_commit(&error),
                "ambiguous write evidence must preserve ownership: {error}"
            );
        }
    }
}
