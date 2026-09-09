//! Filtered note reads preserve the caller's visible scope and insertion cursor.

use khive_storage::note::{Note, NoteFilter};
use khive_storage::{PageRequest, SeekCursor};
use uuid::Uuid;

use crate::{KhiveRuntime, NamespaceToken, RuntimeError, RuntimeResult};

impl KhiveRuntime {
    pub async fn list_notes_filtered(
        &self,
        token: &NamespaceToken,
        mut filter: NoteFilter,
        limit: u32,
        offset: u32,
    ) -> RuntimeResult<Vec<Note>> {
        filter.namespaces = token
            .visible_namespace_strs()
            .into_iter()
            .map(str::to_owned)
            .collect();
        Ok(self
            .notes(token)?
            .query_notes_filtered_count_free(
                token.namespace().as_str(),
                &filter,
                PageRequest {
                    limit,
                    offset: offset.into(),
                },
            )
            .await?
            .items)
    }

    pub async fn list_notes_filtered_after(
        &self,
        token: &NamespaceToken,
        mut filter: NoteFilter,
        after: Option<Uuid>,
        limit: u32,
    ) -> RuntimeResult<(Vec<Note>, Option<Uuid>)> {
        let store = self.notes(token)?;
        let after = match after {
            None => None,
            Some(id) => {
                let note = self
                    .get_note_including_deleted(token, id)
                    .await?
                    .ok_or_else(|| RuntimeError::NotFound(format!("note cursor {id}")))?;
                Self::ensure_namespace_visible(&note.namespace, token)?;
                let sequence = store.note_sequence(id).await?.ok_or_else(|| {
                    RuntimeError::Internal(format!(
                        "note cursor {id} has no insertion-sequence ledger row"
                    ))
                })?;
                Some(SeekCursor { sequence, id })
            }
        };
        filter.namespaces = token
            .visible_namespace_strs()
            .into_iter()
            .map(str::to_owned)
            .collect();
        let page = store
            .query_notes_filtered_after(token.namespace().as_str(), &filter, after, limit)
            .await?;
        Ok((page.items, page.next_after.map(|cursor| cursor.id)))
    }
}
