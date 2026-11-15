//! Note-search's consumer of the memory pack's global note-content graph.

use std::collections::HashSet;

use async_trait::async_trait;
use khive_runtime::note_search_ann::NoteSearchAnnProvider;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, RuntimeResult};
use khive_score::{cmp_desc_then_id, DeterministicScore};
use khive_storage::types::VectorSearchHit;
use uuid::Uuid;

use crate::ann::{self, AnnKey, AnnScoreRoute, FreshTailOutcome, SharedAnn};

pub(crate) struct MemoryNoteSearchAnnProvider {
    runtime: KhiveRuntime,
    ann: SharedAnn,
    backend_id: String,
}

impl MemoryNoteSearchAnnProvider {
    pub(crate) fn new(runtime: KhiveRuntime, ann: SharedAnn) -> Self {
        let backend_id = runtime.backend_id().as_str().to_owned();
        Self {
            runtime: runtime.detached_for_note_search_ann_provider(),
            ann,
            backend_id,
        }
    }

    async fn visible_hits(
        &self,
        token: &NamespaceToken,
        candidates: Vec<(Uuid, f64)>,
    ) -> RuntimeResult<Vec<VectorSearchHit>> {
        let ids: Vec<Uuid> = candidates.iter().map(|(id, _)| *id).collect();
        let live: HashSet<Uuid> = self
            .runtime
            .notes(token)?
            .get_note_visibility_batch(&ids)
            .await?
            .into_iter()
            .filter(|note| {
                note.deleted_at.is_none() && note.namespace == token.namespace().as_str()
            })
            .map(|note| note.id)
            .collect();

        let mut hits: Vec<_> = candidates
            .into_iter()
            .filter(|(id, _)| live.contains(id))
            .map(|(subject_id, cosine)| VectorSearchHit {
                subject_id,
                // The ANN/tail adapter carries the canonical 2^32 fixed-point
                // value losslessly through f64; never round it through f32.
                score: DeterministicScore::from_f64(cosine),
                rank: 0,
            })
            .collect();
        hits.sort_by(|a, b| cmp_desc_then_id(a.score, &a.subject_id, b.score, &b.subject_id));
        Ok(hits)
    }
}

#[async_trait]
impl NoteSearchAnnProvider for MemoryNoteSearchAnnProvider {
    fn backend_id(&self) -> &str {
        &self.backend_id
    }

    fn serves_backend(&self, runtime: &KhiveRuntime) -> bool {
        self.backend_id == runtime.backend_id().as_str()
            && std::ptr::eq(self.runtime.backend(), runtime.backend())
    }

    async fn search(
        &self,
        token: &NamespaceToken,
        model: &str,
        query_embedding: &[f32],
        top_k: u32,
    ) -> RuntimeResult<Option<Vec<VectorSearchHit>>> {
        if top_k == 0 {
            return Ok(Some(Vec::new()));
        }
        let key = AnnKey::new(model);
        ann::maybe_check_durable_epoch(&self.runtime, &self.ann, &key).await;

        // A pre-existing memory graph can have compacted rows before this
        // consumer registered. Only a full checkpoint can make it eligible.
        let active = ann::read_note_search_watermark(&self.runtime, model)
            .await
            .map_err(RuntimeError::Internal)?
            .is_some_and(|watermark| watermark >= 0);
        if !active {
            ann::ensure_ann_background(&self.runtime, token, &self.ann, model).await;
            return Ok(None);
        }

        // Note search always merges the exact fresh tail, independently of the
        // memory/knowledge fresh-tail sampling policy.
        let mut fetch = (top_k as usize)
            .saturating_mul(4)
            .max((top_k as usize).saturating_add(32));
        let rounds = crate::handlers::ann_overfetch_max_rounds(&self.runtime).max(1);
        for round in 0..rounds {
            let Some((candidates, watermark)) = ann::search_loaded_with_seq_route(
                &self.ann,
                &key,
                query_embedding,
                fetch,
                AnnScoreRoute::NoteSearch,
            )
            .await?
            else {
                ann::ensure_ann_background(&self.runtime, token, &self.ann, model).await;
                return Ok(None);
            };
            if !ann::is_current(&self.ann, &key).await {
                ann::ensure_ann_background(&self.runtime, token, &self.ann, model).await;
            }

            let merged = match ann::fresh_tail_serving(
                &self.runtime,
                &self.ann,
                &key,
                model,
                ann::FreshTailSearch::new(query_embedding, fetch, AnnScoreRoute::NoteSearch),
                watermark,
                Some(ann::NOTE_SEARCH_CONSUMER),
            )
            .await
            {
                FreshTailOutcome::Ops(ops) => ann::merge_fresh_tail_for_route(
                    candidates,
                    query_embedding,
                    ops,
                    AnnScoreRoute::NoteSearch,
                )?,
                FreshTailOutcome::Replace(replacement, None) => replacement,
                FreshTailOutcome::Replace(_, Some(reason)) => {
                    return Err(RuntimeError::Internal(reason.into()));
                }
                FreshTailOutcome::Skipped(reason) => {
                    return Err(RuntimeError::Internal(reason.to_string()));
                }
            };
            let mut hits = self.visible_hits(token, merged).await?;
            if hits.len() >= top_k as usize || fetch == usize::MAX || round + 1 == rounds {
                hits.truncate(top_k as usize);
                for (index, hit) in hits.iter_mut().enumerate() {
                    hit.rank = (index + 1) as u32;
                }
                return Ok(Some(hits));
            }
            fetch = fetch.saturating_mul(2);
        }
        unreachable!("overfetch rounds is clamped to at least one")
    }
}

#[cfg(test)]
#[path = "note_search_tests.rs"]
mod tests;
