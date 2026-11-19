use std::collections::BTreeMap;

use khive_storage::note::{NotePropertyPatch, NotePropertyPrecondition};
use khive_storage::types::SqlValue;
use khive_storage::{ContentRef, NewAttachment, Note};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{KhiveRuntime, NamespaceToken, RuntimeResult};

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
        )
        .await
    }

    /// Publish a trusted inbound message and its original-byte attachment in
    /// one database transaction. Channel quarantine must not advertise a
    /// reference in note metadata before GC can see its attachment owner.
    #[allow(clippy::too_many_arguments)]
    pub async fn try_create_note_as_trusted_ingest_with_attachment(
        &self,
        _capability: &crate::pack::ChannelIngestCapability,
        token: &NamespaceToken,
        kind: &str,
        name: Option<&str>,
        content: &str,
        properties: Option<serde_json::Value>,
        attachment: NewAttachment,
        expires_after: Option<std::time::Duration>,
    ) -> RuntimeResult<Option<Note>> {
        self.try_create_note_impl(
            token,
            kind,
            name,
            content,
            properties,
            true,
            Some(attachment),
            expires_after,
        )
        .await
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
}
