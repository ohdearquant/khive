use super::super::StorageBackend;
use super::map_open_error;
use async_trait::async_trait;
use khive_storage::note::{Note, NoteFilter, NoteKeyCursor, NoteStore, NoteVisibility};
use khive_storage::{
    BatchWriteSummary, BoundedCount, DeleteMode, Page, PageRequest, SeekCursor, SeekPage,
    StorageCapability, StorageResult,
};
use serde_json::Value;
use uuid::Uuid;

#[async_trait]
impl NoteStore for StorageBackend {
    async fn get_live_notes_by_key(
        &self,
        _namespace: &str,
        _key: &str,
        _kind: Option<&str>,
    ) -> StorageResult<Vec<Note>> {
        self.notes()
            .map_err(|error| map_open_error(error, StorageCapability::Notes, "notes"))?
            .get_live_notes_by_key(_namespace, _key, _kind)
            .await
    }

    async fn query_keyed_notes(
        &self,
        _namespace: &str,
        _filter: &NoteFilter,
        _prefix: &str,
        _after: Option<&NoteKeyCursor>,
        _page: PageRequest,
    ) -> StorageResult<(Vec<Note>, Option<NoteKeyCursor>)> {
        self.notes()
            .map_err(|error| map_open_error(error, StorageCapability::Notes, "notes"))?
            .query_keyed_notes(_namespace, _filter, _prefix, _after, _page)
            .await
    }

    async fn upsert_note(&self, note: Note) -> StorageResult<()> {
        self.notes()
            .map_err(|error| map_open_error(error, StorageCapability::Notes, "notes"))?
            .upsert_note(note)
            .await
    }

    async fn replace_note_if_unchanged(
        &self,
        _note: Note,
        _expected_updated_at: i64,
        _expected_deleted_at: Option<i64>,
    ) -> StorageResult<bool> {
        self.notes()
            .map_err(|error| map_open_error(error, StorageCapability::Notes, "notes"))?
            .replace_note_if_unchanged(_note, _expected_updated_at, _expected_deleted_at)
            .await
    }

    async fn insert_note_if_absent(&self, _note: Note) -> StorageResult<bool> {
        self.notes()
            .map_err(|error| map_open_error(error, StorageCapability::Notes, "notes"))?
            .insert_note_if_absent(_note)
            .await
    }

    async fn upsert_notes(&self, notes: Vec<Note>) -> StorageResult<BatchWriteSummary> {
        self.notes()
            .map_err(|error| map_open_error(error, StorageCapability::Notes, "notes"))?
            .upsert_notes(notes)
            .await
    }

    async fn get_note(&self, id: Uuid) -> StorageResult<Option<Note>> {
        self.notes()
            .map_err(|error| map_open_error(error, StorageCapability::Notes, "notes"))?
            .get_note(id)
            .await
    }

    async fn get_note_including_deleted(&self, id: Uuid) -> StorageResult<Option<Note>> {
        self.notes()
            .map_err(|error| map_open_error(error, StorageCapability::Notes, "notes"))?
            .get_note_including_deleted(id)
            .await
    }

    async fn delete_note(&self, id: Uuid, mode: DeleteMode) -> StorageResult<bool> {
        self.notes()
            .map_err(|error| map_open_error(error, StorageCapability::Notes, "notes"))?
            .delete_note(id, mode)
            .await
    }

    async fn update_note_properties(
        &self,
        id: Uuid,
        properties: Option<Value>,
        updated_at: i64,
    ) -> StorageResult<bool> {
        self.notes()
            .map_err(|error| map_open_error(error, StorageCapability::Notes, "notes"))?
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
        self.notes()
            .map_err(|error| map_open_error(error, StorageCapability::Notes, "notes"))?
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
        self.notes()
            .map_err(|error| map_open_error(error, StorageCapability::Notes, "notes"))?
            .try_patch_note_property(id, namespace, filter, json_path, value, updated_at)
            .await
    }

    async fn patch_note_property_atomic(
        &self,
        _ids: Vec<Uuid>,
        _namespace: &str,
        _filter: &NoteFilter,
        _json_path: &str,
        _value: Value,
        _updated_at: i64,
    ) -> StorageResult<()> {
        self.notes()
            .map_err(|error| map_open_error(error, StorageCapability::Notes, "notes"))?
            .patch_note_property_atomic(_ids, _namespace, _filter, _json_path, _value, _updated_at)
            .await
    }

    async fn query_notes(
        &self,
        namespace: &str,
        kind: Option<&str>,
        page: PageRequest,
    ) -> StorageResult<Page<Note>> {
        self.notes()
            .map_err(|error| map_open_error(error, StorageCapability::Notes, "notes"))?
            .query_notes(namespace, kind, page)
            .await
    }

    async fn query_notes_count_free(
        &self,
        _namespace: &str,
        _kind: Option<&str>,
        _page: PageRequest,
    ) -> StorageResult<Page<Note>> {
        self.notes()
            .map_err(|error| map_open_error(error, StorageCapability::Notes, "notes"))?
            .query_notes_count_free(_namespace, _kind, _page)
            .await
    }

    async fn query_notes_filtered(
        &self,
        namespace: &str,
        filter: &NoteFilter,
        page: PageRequest,
    ) -> StorageResult<Page<Note>> {
        self.notes()
            .map_err(|error| map_open_error(error, StorageCapability::Notes, "notes"))?
            .query_notes_filtered(namespace, filter, page)
            .await
    }

    async fn query_notes_filtered_count_free(
        &self,
        _namespace: &str,
        _filter: &NoteFilter,
        _page: PageRequest,
    ) -> StorageResult<Page<Note>> {
        self.notes()
            .map_err(|error| map_open_error(error, StorageCapability::Notes, "notes"))?
            .query_notes_filtered_count_free(_namespace, _filter, _page)
            .await
    }

    async fn count_notes_filtered_in_snapshot(
        &self,
        _namespace: &str,
        _filters: &[NoteFilter],
    ) -> StorageResult<Vec<u64>> {
        self.notes()
            .map_err(|error| map_open_error(error, StorageCapability::Notes, "notes"))?
            .count_notes_filtered_in_snapshot(_namespace, _filters)
            .await
    }

    async fn count_notes_filtered_bounded_in_snapshot(
        &self,
        _namespace: &str,
        _filters: &[NoteFilter],
        _cap: u32,
    ) -> StorageResult<Vec<BoundedCount>> {
        self.notes()
            .map_err(|error| map_open_error(error, StorageCapability::Notes, "notes"))?
            .count_notes_filtered_bounded_in_snapshot(_namespace, _filters, _cap)
            .await
    }

    async fn note_sequence(&self, _id: Uuid) -> StorageResult<Option<i64>> {
        self.notes()
            .map_err(|error| map_open_error(error, StorageCapability::Notes, "notes"))?
            .note_sequence(_id)
            .await
    }

    async fn query_notes_filtered_after(
        &self,
        _namespace: &str,
        _filter: &NoteFilter,
        _after: Option<SeekCursor>,
        _limit: u32,
    ) -> StorageResult<SeekPage<Note>> {
        self.notes()
            .map_err(|error| map_open_error(error, StorageCapability::Notes, "notes"))?
            .query_notes_filtered_after(_namespace, _filter, _after, _limit)
            .await
    }

    async fn query_notes_filtered_bounded(
        &self,
        namespace: &str,
        filter: &NoteFilter,
        max_rows: u32,
    ) -> StorageResult<Vec<Note>> {
        self.notes()
            .map_err(|error| map_open_error(error, StorageCapability::Notes, "notes"))?
            .query_notes_filtered_bounded(namespace, filter, max_rows)
            .await
    }

    async fn count_notes(&self, namespace: &str, kind: Option<&str>) -> StorageResult<u64> {
        self.notes()
            .map_err(|error| map_open_error(error, StorageCapability::Notes, "notes"))?
            .count_notes(namespace, kind)
            .await
    }

    async fn count_notes_in_namespaces(
        &self,
        namespaces: &[String],
        kind: Option<&str>,
    ) -> StorageResult<u64> {
        self.notes()
            .map_err(|error| map_open_error(error, StorageCapability::Notes, "notes"))?
            .count_notes_in_namespaces(namespaces, kind)
            .await
    }

    async fn try_insert_note(&self, note: Note) -> StorageResult<bool> {
        self.notes()
            .map_err(|error| map_open_error(error, StorageCapability::Notes, "notes"))?
            .try_insert_note(note)
            .await
    }

    async fn try_insert_note_with_attachments(
        &self,
        _note: Note,
        _attachments: Vec<khive_storage::Attachment>,
    ) -> StorageResult<bool> {
        self.notes()
            .map_err(|error| map_open_error(error, StorageCapability::Notes, "notes"))?
            .try_insert_note_with_attachments(_note, _attachments)
            .await
    }

    async fn get_notes_batch(&self, ids: &[Uuid]) -> StorageResult<Vec<Note>> {
        self.notes()
            .map_err(|error| map_open_error(error, StorageCapability::Notes, "notes"))?
            .get_notes_batch(ids)
            .await
    }

    async fn get_notes_batch_including_deleted(&self, ids: &[Uuid]) -> StorageResult<Vec<Note>> {
        self.notes()
            .map_err(|error| map_open_error(error, StorageCapability::Notes, "notes"))?
            .get_notes_batch_including_deleted(ids)
            .await
    }

    async fn get_note_visibility_batch(&self, ids: &[Uuid]) -> StorageResult<Vec<NoteVisibility>> {
        self.notes()
            .map_err(|error| map_open_error(error, StorageCapability::Notes, "notes"))?
            .get_note_visibility_batch(ids)
            .await
    }
}
