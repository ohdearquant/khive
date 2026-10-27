//! Filtered note reads preserve the caller's visible scope and insertion cursor.

use khive_storage::note::{Note, NoteFilter, NoteStatusSelector};
use khive_storage::{PageRequest, SeekCursor};
use uuid::Uuid;

use crate::{KhiveRuntime, MailboxView, NamespaceToken, RuntimeError, RuntimeResult};

impl KhiveRuntime {
    pub async fn list_notes_filtered(
        &self,
        token: &NamespaceToken,
        mut filter: NoteFilter,
        limit: u32,
        offset: u32,
    ) -> RuntimeResult<Vec<Note>> {
        filter = filter.with_default_note_status(NoteStatusSelector::Active);
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
        filter = filter.with_default_note_status(NoteStatusSelector::Active);
        let store = self.notes(token)?;
        let after = match after {
            None => None,
            Some(id) => {
                let note = self
                    .get_note_including_deleted(token, id)
                    .await?
                    .ok_or_else(|| RuntimeError::NotFound(format!("note cursor {id}")))?;
                Self::ensure_namespace_visible(&note.namespace, token)?;
                // A cursor naming a message the mailbox scope hides answers
                // exactly as a missing cursor does, so it is not an
                // existence probe.
                if filter
                    .mailbox
                    .as_ref()
                    .is_some_and(|scope| !MailboxView::scope_permits_message_note(scope, &note))
                {
                    return Err(RuntimeError::NotFound(format!("note cursor {id}")));
                }
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

#[cfg(test)]
mod archive_visibility_tests {
    use super::*;
    use crate::Namespace;
    use std::time::Duration;

    async fn fixture() -> (KhiveRuntime, NamespaceToken, [Uuid; 4]) {
        let runtime = KhiveRuntime::memory().unwrap();
        let token = runtime.authorize(Namespace::local()).unwrap();
        let mut ids = Vec::new();
        for (index, status) in ["archived", "active", "archived", "active"]
            .into_iter()
            .enumerate()
        {
            let mut note = Note::new("local", "observation", format!("archive probe {index}"));
            note.status = status.into();
            note.created_at = 40 - index as i64 * 10;
            note.updated_at = note.created_at;
            // Deliberately opposite workflow metadata: filtering must use the column.
            note.properties = Some(
                serde_json::json!({"status": if status == "active" { "archived" } else { "active" }}),
            );
            ids.push(note.id);
            runtime
                .notes(&token)
                .unwrap()
                .upsert_note(note)
                .await
                .unwrap();
        }
        (runtime, token, ids.try_into().unwrap())
    }

    fn ids(notes: &[Note]) -> Vec<Uuid> {
        notes.iter().map(|note| note.id).collect()
    }

    #[tokio::test]
    async fn ordinary_runtime_lists_default_active_before_offset_and_seek() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (runtime, token, note_ids) = fixture().await;
            for offset in [0, 1] {
                let expected = vec![note_ids[if offset == 0 { 1 } else { 3 }]];
                assert_eq!(
                    ids(&runtime.list_notes(&token, None, 1, offset).await.unwrap()),
                    expected,
                    "ordinary Runtime list must default to the active column before pagination"
                );
                assert_eq!(
                    ids(&runtime
                        .list_notes_filtered(&token, NoteFilter::default(), 1, offset)
                        .await
                        .unwrap()),
                    expected
                );
            }
            let (first, next) = runtime
                .list_notes_after(&token, None, None, 1)
                .await
                .unwrap();
            assert_eq!(ids(&first), vec![note_ids[1]]);
            assert_eq!(next, Some(note_ids[1]));
            let (filtered, filtered_next) = runtime
                .list_notes_filtered_after(&token, NoteFilter::default(), None, 1)
                .await
                .unwrap();
            assert_eq!(ids(&filtered), vec![note_ids[1]]);
            assert_eq!(filtered_next, Some(note_ids[1]));
            // An explicit archived cursor is still a valid insertion boundary.
            let (after, _) = runtime
                .list_notes_after(&token, None, Some(note_ids[0]), 10)
                .await
                .unwrap();
            assert_eq!(ids(&after), vec![note_ids[1], note_ids[3]]);
            let (after, _) = runtime
                .list_notes_filtered_after(&token, NoteFilter::default(), Some(note_ids[0]), 10)
                .await
                .unwrap();
            assert_eq!(ids(&after), vec![note_ids[1], note_ids[3]]);
            assert_eq!(
                runtime
                    .get_note_including_deleted(&token, note_ids[0])
                    .await
                    .unwrap()
                    .unwrap()
                    .status,
                "archived"
            );
        })
        .await
        .expect("bounded actual runtime list regression");
    }

    #[tokio::test]
    async fn runtime_explicit_archived_and_any_preserve_raw_population_and_column_status() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (runtime, token, note_ids) = fixture().await;
            for (selector, expected) in [
                (NoteStatusSelector::Archived, vec![note_ids[0], note_ids[2]]),
                (NoteStatusSelector::Any, note_ids.to_vec()),
            ] {
                let filter = NoteFilter {
                    note_status: Some(selector),
                    ..Default::default()
                };
                assert_eq!(
                    ids(&runtime
                        .list_notes_filtered(&token, filter.clone(), 10, 0)
                        .await
                        .unwrap()),
                    expected
                );
                let (notes, _) = runtime
                    .list_notes_filtered_after(&token, filter, None, 10)
                    .await
                    .unwrap();
                assert_eq!(ids(&notes), expected);
            }
            let raw = runtime
                .notes(&token)
                .unwrap()
                .query_notes_count_free(
                    "local",
                    None,
                    PageRequest {
                        limit: 10,
                        offset: 0,
                    },
                )
                .await
                .unwrap();
            assert_eq!(
                raw.items.len(),
                4,
                "raw operational read remains status-neutral"
            );
            assert_eq!(raw.total, None, "count-free contract is unchanged");
            let active = runtime.list_notes(&token, None, 10, 0).await.unwrap();
            assert!(active.iter().all(|note| note.status == "active"
                && note.properties.as_ref().unwrap()["status"] == "archived"));
        })
        .await
        .expect("bounded explicit-selector regression");
    }

    #[tokio::test]
    async fn active_list_defaults_preserve_the_visible_namespace_set() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let runtime = KhiveRuntime::memory().unwrap();
            for namespace in ["archive-a", "archive-b", "archive-c"] {
                let token = runtime
                    .authorize(Namespace::parse(namespace).unwrap())
                    .unwrap();
                for status in ["active", "archived"] {
                    let mut note = Note::new(namespace, "observation", status);
                    note.status = status.into();
                    runtime
                        .notes(&token)
                        .unwrap()
                        .upsert_note(note)
                        .await
                        .unwrap();
                }
            }
            let token = runtime
                .authorize_with_visibility(
                    Namespace::parse("archive-a").unwrap(),
                    vec![Namespace::parse("archive-b").unwrap()],
                )
                .unwrap();
            let filter = NoteFilter {
                namespaces: vec!["archive-c".into()],
                ..Default::default()
            };
            let lists = [
                runtime.list_notes(&token, None, 10, 0).await.unwrap(),
                runtime
                    .list_notes_filtered(&token, filter.clone(), 10, 0)
                    .await
                    .unwrap(),
                runtime
                    .list_notes_after(&token, None, None, 10)
                    .await
                    .unwrap()
                    .0,
                runtime
                    .list_notes_filtered_after(&token, filter, None, 10)
                    .await
                    .unwrap()
                    .0,
            ];
            for notes in lists {
                assert_eq!(notes.len(), 2);
                assert!(notes.iter().all(|note| note.status == "active"
                    && matches!(note.namespace.as_str(), "archive-a" | "archive-b")));
            }
            let all = runtime
                .list_notes_filtered(
                    &token,
                    NoteFilter {
                        note_status: Some(NoteStatusSelector::Any),
                        ..Default::default()
                    },
                    10,
                    0,
                )
                .await
                .unwrap();
            assert_eq!(all.len(), 4);
            assert!(all.iter().all(|note| note.namespace != "archive-c"));
        })
        .await
        .expect("bounded visible-namespace regression");
    }
}
