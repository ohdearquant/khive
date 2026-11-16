use std::cmp::Reverse;
use std::collections::HashMap;

use khive_pack_kg::handlers::{SearchOrder, ValidatedSearchRequest};
use khive_runtime::error::SearchReadSubstrate;
use khive_runtime::{Namespace, NoteSearchHit, RuntimeError, RuntimeResult, SearchHit};
use khive_score::DeterministicScore;

use super::{ensure_search_active, SubstrateCoordinator};

impl SubstrateCoordinator {
    pub(super) async fn finalize_search_hits(
        &self,
        entity_hits: &mut Vec<SearchHit>,
        note_hits: &mut Vec<NoteSearchHit>,
        request: &ValidatedSearchRequest,
        namespace: &Namespace,
    ) -> RuntimeResult<()> {
        ensure_search_active()?;
        let floor = DeterministicScore::from_f64(request.min_score());
        entity_hits.retain(|hit| hit.score >= floor);
        note_hits.retain(|hit| hit.score >= floor);

        if request.order_by() != SearchOrder::Score || request.has_created_range() {
            let mut entity_times = HashMap::new();
            for hit in entity_hits.iter() {
                ensure_search_active()?;
                let backend_id = self
                    .locate_for_search(hit.entity_id, namespace, SearchReadSubstrate::Entity)
                    .await;
                ensure_search_active()?;
                if let Some(backend_id) = backend_id? {
                    if let Some(entry) = self.registry().get(&backend_id) {
                        let contextualize = |error: RuntimeError| {
                            error.with_search_read_context(
                                backend_id.clone(),
                                SearchReadSubstrate::Entity,
                            )
                        };
                        let token = entry
                            .runtime
                            .authorize(namespace.clone())
                            .map_err(contextualize)?;
                        #[cfg(test)]
                        super::before_search_metadata_read(
                            super::SearchMetadataPhase::Finalize,
                            khive_pack_kg::handlers::SearchSubstrate::Entity,
                            hit.entity_id,
                        )
                        .await;
                        let entity = entry.runtime.get_entity(&token, hit.entity_id).await;
                        ensure_search_active()?;
                        match entity {
                            Ok(entity) => {
                                entity_times
                                    .insert(hit.entity_id, (entity.created_at, entity.updated_at));
                                #[cfg(test)]
                                super::notify_search_metadata_read(
                                    super::SearchMetadataPhase::Finalize,
                                    khive_pack_kg::handlers::SearchSubstrate::Entity,
                                    hit.entity_id,
                                );
                                ensure_search_active()?;
                            }
                            Err(RuntimeError::NotFound(_)) => {}
                            Err(error) => return Err(contextualize(error)),
                        }
                    }
                }
            }
            let mut note_times = HashMap::new();
            for hit in note_hits.iter() {
                ensure_search_active()?;
                let backend_id = self
                    .locate_for_search(hit.note_id, namespace, SearchReadSubstrate::Note)
                    .await;
                ensure_search_active()?;
                if let Some(backend_id) = backend_id? {
                    if let Some(entry) = self.registry().get(&backend_id) {
                        let contextualize = |error: RuntimeError| {
                            error.with_search_read_context(
                                backend_id.clone(),
                                SearchReadSubstrate::Note,
                            )
                        };
                        let token = entry
                            .runtime
                            .authorize(namespace.clone())
                            .map_err(contextualize)?;
                        let store = entry.runtime.notes(&token).map_err(contextualize)?;
                        #[cfg(test)]
                        super::before_search_metadata_read(
                            super::SearchMetadataPhase::Finalize,
                            khive_pack_kg::handlers::SearchSubstrate::Note,
                            hit.note_id,
                        )
                        .await;
                        let note = store.get_note(hit.note_id).await;
                        ensure_search_active()?;
                        if let Some(note) = note.map_err(|error| contextualize(error.into()))? {
                            note_times.insert(hit.note_id, (note.created_at, note.updated_at));
                            #[cfg(test)]
                            super::notify_search_metadata_read(
                                super::SearchMetadataPhase::Finalize,
                                khive_pack_kg::handlers::SearchSubstrate::Note,
                                hit.note_id,
                            );
                            ensure_search_active()?;
                        }
                    }
                }
            }
            ensure_search_active()?;
            if request.has_created_range() {
                entity_hits.retain(|hit| {
                    entity_times
                        .get(&hit.entity_id)
                        .is_some_and(|&(created, _)| request.matches_created_at(created))
                });
                note_hits.retain(|hit| {
                    note_times
                        .get(&hit.note_id)
                        .is_some_and(|&(created, _)| request.matches_created_at(created))
                });
            }
            if request.order_by() != SearchOrder::Score {
                // Stable sorting retains incoming rank/UUID order for equal or
                // unavailable timestamps, matching the single-runtime search path.
                entity_hits.sort_by_key(|hit| {
                    Reverse(
                        entity_times.get(&hit.entity_id).map(|&(created, updated)| {
                            timestamp(request.order_by(), created, updated)
                        }),
                    )
                });
                note_hits.sort_by_key(|hit| {
                    Reverse(
                        note_times.get(&hit.note_id).map(|&(created, updated)| {
                            timestamp(request.order_by(), created, updated)
                        }),
                    )
                });
            }
        }
        ensure_search_active()?;
        entity_hits.truncate(request.limit() as usize);
        note_hits.truncate(request.limit() as usize);
        ensure_search_active()
    }
}

fn timestamp(order: SearchOrder, created_at: i64, updated_at: i64) -> i64 {
    match order {
        SearchOrder::CreatedAt => created_at,
        SearchOrder::UpdatedAt => updated_at,
        SearchOrder::Score => unreachable!("score order does not fetch timestamps"),
    }
}
