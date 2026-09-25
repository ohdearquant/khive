use std::cmp::Reverse;
use std::collections::HashMap;

use khive_pack_kg::handlers::{SearchOrder, ValidatedSearchRequest};
use khive_runtime::{Namespace, NoteSearchHit, SearchHit};
use khive_score::DeterministicScore;

use super::SubstrateCoordinator;

impl SubstrateCoordinator {
    pub(super) async fn finalize_search_hits(
        &self,
        entity_hits: &mut Vec<SearchHit>,
        note_hits: &mut Vec<NoteSearchHit>,
        request: &ValidatedSearchRequest,
        namespace: &Namespace,
    ) {
        let floor = DeterministicScore::from_f64(request.min_score());
        entity_hits.retain(|hit| hit.score >= floor);
        note_hits.retain(|hit| hit.score >= floor);

        if request.order_by() != SearchOrder::Score {
            let mut entity_times = HashMap::new();
            for hit in entity_hits.iter() {
                if khive_storage::request_read_is_cancelled() {
                    break;
                }
                if let Some(backend_id) = self.locate(hit.entity_id, namespace).await {
                    if let Some(entry) = self.registry().get(&backend_id) {
                        if let Ok(token) = entry.runtime.authorize(namespace.clone()) {
                            if let Ok(entity) =
                                entry.runtime.get_entity(&token, hit.entity_id).await
                            {
                                entity_times.insert(
                                    hit.entity_id,
                                    timestamp(
                                        request.order_by(),
                                        entity.created_at,
                                        entity.updated_at,
                                    ),
                                );
                            }
                        }
                    }
                }
            }
            let mut note_times = HashMap::new();
            for hit in note_hits.iter() {
                if khive_storage::request_read_is_cancelled() {
                    break;
                }
                if let Some(backend_id) = self.locate(hit.note_id, namespace).await {
                    if let Some(entry) = self.registry().get(&backend_id) {
                        if let Ok(token) = entry.runtime.authorize(namespace.clone()) {
                            if let Ok(store) = entry.runtime.notes(&token) {
                                if let Ok(Some(note)) = store.get_note(hit.note_id).await {
                                    note_times.insert(
                                        hit.note_id,
                                        timestamp(
                                            request.order_by(),
                                            note.created_at,
                                            note.updated_at,
                                        ),
                                    );
                                }
                            }
                        }
                    }
                }
            }
            // Stable sorting retains incoming rank/UUID order for equal or
            // unavailable timestamps, matching the single-runtime search path.
            entity_hits.sort_by_key(|hit| Reverse(entity_times.get(&hit.entity_id).copied()));
            note_hits.sort_by_key(|hit| Reverse(note_times.get(&hit.note_id).copied()));
        }
        entity_hits.truncate(request.limit() as usize);
        note_hits.truncate(request.limit() as usize);
    }
}

fn timestamp(order: SearchOrder, created_at: i64, updated_at: i64) -> i64 {
    match order {
        SearchOrder::CreatedAt => created_at,
        SearchOrder::UpdatedAt => updated_at,
        SearchOrder::Score => unreachable!("score order does not fetch timestamps"),
    }
}
