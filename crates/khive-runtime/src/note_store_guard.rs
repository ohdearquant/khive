//! Policy-enforcing decorator around the [`NoteStore`] returned by the public
//! [`crate::KhiveRuntime::notes`] accessor.
//!
//! `comm.health` (`khive-pack-comm`) counts every live `kind = "message"` row
//! whose JSON `properties` carries a truthy `quarantined` value, grouped by
//! `channel_kind`/`channel_slug`. Those three keys are transport-owned
//! evidence that only the trusted channel-ingest path
//! (`KhiveRuntime::try_create_note_as_trusted_ingest`) may establish. The
//! generic `create`/`update` verb funnel and the proposal-apply path already
//! run a pack-installed note-write validator that refuses them, but that
//! validator does not cover a caller reaching `notes(&token)` directly and
//! calling the raw storage insert/upsert methods — this decorator closes
//! that seam by refusing the same three properties on a `kind = "message"`
//! note at every full-note write.
//!
//! `try_create_note_impl` (the runtime-internal implementation backing both
//! `try_create_note` and `try_create_note_as_trusted_ingest`) bypasses this
//! decorator entirely by reaching the backend directly
//! (`KhiveRuntime::raw_notes`) rather than through `notes()`; it enforces the
//! identical property check itself, conditionally allowing the three keys
//! only when called with a `ChannelIngestCapability`. That keeps the trusted
//! ingest path exempt by construction rather than by call site.

use std::sync::Arc;

use async_trait::async_trait;
use khive_storage::{
    BatchWriteSummary, DeleteMode, Note, NoteFilter, NoteStore, Page, PageRequest, SeekCursor,
    SeekPage, StorageCapability, StorageError, StorageResult,
};
use serde_json::Value;
use uuid::Uuid;

const TRANSPORT_OWNED_MESSAGE_PROPERTIES: &[&str] =
    &["quarantined", "channel_kind", "channel_slug"];

fn transport_owned_message_property_named_in(
    properties: &serde_json::Map<String, Value>,
) -> Option<&'static str> {
    TRANSPORT_OWNED_MESSAGE_PROPERTIES
        .iter()
        .copied()
        .find(|key| properties.contains_key(*key))
}

fn reject_if_forged_message_note(note: &Note, operation: &'static str) -> StorageResult<()> {
    if note.kind != "message" {
        return Ok(());
    }
    let Some(key) = note
        .properties
        .as_ref()
        .and_then(Value::as_object)
        .and_then(transport_owned_message_property_named_in)
    else {
        return Ok(());
    };
    Err(StorageError::InvalidInput {
        capability: StorageCapability::Notes,
        operation: operation.into(),
        message: format!(
            "`{key}` is transport-owned on a `message` note and cannot be written through the \
             public NoteStore accessor; only the trusted channel-ingest path may establish \
             quarantine disposition and channel provenance"
        ),
    })
}

/// The property-patch seams cannot see the note's kind in their signatures,
/// so they refuse the reserved keys unconditionally: no public-store caller
/// legitimately patches transport-owned keys on any note kind (quarantine
/// disposition is established only at ingest, through the capability path),
/// and a kind-scoped check would need a lookup whose result the refusal
/// would then depend on. A future release-from-quarantine flow belongs on
/// the capability path, not here.
fn reject_reserved_patch_target(target: &str, operation: &'static str) -> StorageResult<()> {
    let first_segment = target
        .strip_prefix("$.")
        .unwrap_or(target)
        .split(['.', '['])
        .next()
        .unwrap_or(target);
    if !TRANSPORT_OWNED_MESSAGE_PROPERTIES.contains(&first_segment) {
        return Ok(());
    }
    Err(StorageError::InvalidInput {
        capability: StorageCapability::Notes,
        operation: operation.into(),
        message: format!(
            "`{first_segment}` is transport-owned and cannot be patched through the public \
             NoteStore accessor; only the trusted channel-ingest path may establish quarantine \
             disposition and channel provenance"
        ),
    })
}

fn reject_reserved_replacement_properties(
    properties: Option<&Value>,
    operation: &'static str,
) -> StorageResult<()> {
    let Some(key) = properties
        .and_then(Value::as_object)
        .and_then(transport_owned_message_property_named_in)
    else {
        return Ok(());
    };
    Err(StorageError::InvalidInput {
        capability: StorageCapability::Notes,
        operation: operation.into(),
        message: format!(
            "`{key}` is transport-owned and cannot be written through the public NoteStore \
             accessor; only the trusted channel-ingest path may establish quarantine disposition \
             and channel provenance"
        ),
    })
}

/// Wraps `inner` so every full-note insert/upsert seam AND every
/// property-patch seam enforces the reserved-transport-property policy
/// described at module level. Insert/upsert refusal is scoped to
/// `kind = "message"` notes (the note is in hand); the patch seams refuse
/// the reserved keys on any note, since kind is not in their signatures and
/// no public-store caller legitimately patches those keys at all.
pub(crate) struct PolicyEnforcingNoteStore {
    inner: Arc<dyn NoteStore>,
}

impl PolicyEnforcingNoteStore {
    pub(crate) fn wrap(inner: Arc<dyn NoteStore>) -> Arc<dyn NoteStore> {
        Arc::new(Self { inner })
    }
}

#[async_trait]
impl NoteStore for PolicyEnforcingNoteStore {
    async fn upsert_note(&self, note: Note) -> StorageResult<()> {
        reject_if_forged_message_note(&note, "upsert_note")?;
        self.inner.upsert_note(note).await
    }

    async fn replace_note_if_unchanged(
        &self,
        note: Note,
        expected_updated_at: i64,
        expected_deleted_at: Option<i64>,
    ) -> StorageResult<bool> {
        reject_if_forged_message_note(&note, "replace_note_if_unchanged")?;
        self.inner
            .replace_note_if_unchanged(note, expected_updated_at, expected_deleted_at)
            .await
    }

    async fn upsert_notes(&self, notes: Vec<Note>) -> StorageResult<BatchWriteSummary> {
        for note in &notes {
            reject_if_forged_message_note(note, "upsert_notes")?;
        }
        self.inner.upsert_notes(notes).await
    }

    async fn get_note(&self, id: Uuid) -> StorageResult<Option<Note>> {
        self.inner.get_note(id).await
    }

    async fn get_note_including_deleted(&self, id: Uuid) -> StorageResult<Option<Note>> {
        self.inner.get_note_including_deleted(id).await
    }

    async fn delete_note(&self, id: Uuid, mode: DeleteMode) -> StorageResult<bool> {
        self.inner.delete_note(id, mode).await
    }

    async fn update_note_properties(
        &self,
        id: Uuid,
        properties: Option<Value>,
        updated_at: i64,
    ) -> StorageResult<bool> {
        reject_reserved_replacement_properties(properties.as_ref(), "update_note_properties")?;
        self.inner
            .update_note_properties(id, properties, updated_at)
            .await
    }

    async fn set_note_property(
        &self,
        id: Uuid,
        key: &str,
        value: Value,
        updated_at: i64,
    ) -> StorageResult<bool> {
        reject_reserved_patch_target(key, "set_note_property")?;
        self.inner
            .set_note_property(id, key, value, updated_at)
            .await
    }

    async fn try_patch_note_property(
        &self,
        id: Uuid,
        namespace: &str,
        filter: &NoteFilter,
        json_path: &str,
        value: Value,
        updated_at: i64,
    ) -> StorageResult<bool> {
        reject_reserved_patch_target(json_path, "try_patch_note_property")?;
        self.inner
            .try_patch_note_property(id, namespace, filter, json_path, value, updated_at)
            .await
    }

    async fn patch_note_property_atomic(
        &self,
        ids: Vec<Uuid>,
        namespace: &str,
        filter: &NoteFilter,
        json_path: &str,
        value: Value,
        updated_at: i64,
    ) -> StorageResult<()> {
        reject_reserved_patch_target(json_path, "patch_note_property_atomic")?;
        self.inner
            .patch_note_property_atomic(ids, namespace, filter, json_path, value, updated_at)
            .await
    }

    async fn query_notes(
        &self,
        namespace: &str,
        kind: Option<&str>,
        page: PageRequest,
    ) -> StorageResult<Page<Note>> {
        self.inner.query_notes(namespace, kind, page).await
    }

    async fn query_notes_filtered(
        &self,
        namespace: &str,
        filter: &NoteFilter,
        page: PageRequest,
    ) -> StorageResult<Page<Note>> {
        self.inner
            .query_notes_filtered(namespace, filter, page)
            .await
    }

    async fn note_sequence(&self, id: Uuid) -> StorageResult<Option<i64>> {
        self.inner.note_sequence(id).await
    }

    async fn query_notes_filtered_after(
        &self,
        namespace: &str,
        filter: &NoteFilter,
        after: Option<SeekCursor>,
        limit: u32,
    ) -> StorageResult<SeekPage<Note>> {
        self.inner
            .query_notes_filtered_after(namespace, filter, after, limit)
            .await
    }

    async fn query_notes_filtered_bounded(
        &self,
        namespace: &str,
        filter: &NoteFilter,
        max_rows: u32,
    ) -> StorageResult<Vec<Note>> {
        self.inner
            .query_notes_filtered_bounded(namespace, filter, max_rows)
            .await
    }

    async fn count_notes(&self, namespace: &str, kind: Option<&str>) -> StorageResult<u64> {
        self.inner.count_notes(namespace, kind).await
    }

    async fn count_notes_in_namespaces(
        &self,
        namespaces: &[String],
        kind: Option<&str>,
    ) -> StorageResult<u64> {
        self.inner.count_notes_in_namespaces(namespaces, kind).await
    }

    async fn try_insert_note(&self, note: Note) -> StorageResult<bool> {
        reject_if_forged_message_note(&note, "try_insert_note")?;
        self.inner.try_insert_note(note).await
    }

    async fn get_notes_batch(&self, ids: &[Uuid]) -> StorageResult<Vec<Note>> {
        self.inner.get_notes_batch(ids).await
    }
}
