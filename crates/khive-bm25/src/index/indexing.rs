//! Document indexing operations for BM25 index.

use std::collections::BTreeMap;

use super::{Bm25Index, DocumentId};
use crate::error::{Result, RetrievalError};
use crate::metrics::{self, MetricEvent, MetricValue};

impl Bm25Index {
    /// Index a document. Re-indexes if already present; budget check skipped for re-index.
    pub fn index_document(&mut self, doc_id: impl Into<DocumentId>, text: &str) -> Result<()> {
        let start = std::time::Instant::now();

        let result = self.index_document_inner(doc_id, text);

        // Emit metrics
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        metrics::emit(
            &self.metrics,
            MetricEvent {
                name: metrics::names::BM25_INDEX_DURATION_MS,
                value: MetricValue::Histogram(elapsed),
                labels: vec![],
            },
        );
        metrics::emit(
            &self.metrics,
            MetricEvent {
                name: metrics::names::BM25_INDEX_COUNT,
                value: MetricValue::Counter(1),
                labels: vec![],
            },
        );
        metrics::emit(
            &self.metrics,
            MetricEvent {
                name: metrics::names::BM25_INDEX_SIZE,
                value: MetricValue::Gauge(self.doc_count() as f64),
                labels: vec![],
            },
        );

        result
    }

    /// Inner `index_document` logic: tokenize first, mutate second (prevents half-mutated state).
    fn index_document_inner(&mut self, doc_id: impl Into<DocumentId>, text: &str) -> Result<()> {
        let doc_id: DocumentId = doc_id.into();
        // Check if this is a re-index (bypass budget for existing docs)
        let is_reindex = self.contains_document(&doc_id);

        // Phase 1: tokenize and compute term frequencies BEFORE any mutation.
        let tokens = self.tokenizer.tokenize(text);
        let doc_length = tokens.len();

        if doc_length == 0 {
            // Don't index empty documents; preserve existing document if re-indexing.
            return Ok(());
        }

        // Count term frequencies (collected before any mutation).
        let mut term_freqs: BTreeMap<String, u32> = BTreeMap::new();
        for token in &tokens {
            *term_freqs.entry(token.clone()).or_insert(0) += 1;
        }

        // Budget check for new documents only (re-index bypasses).
        // Performed after tokenization but before any mutation.
        if !is_reindex {
            if let Some(limit) = self.config.memory_budget {
                let current = self.memory_usage();
                let cost = self.estimate_document_cost(text);
                if current.saturating_add(cost) > limit {
                    return Err(RetrievalError::budget_exceeded(current, cost, limit));
                }
            }
        }

        // Phase 2: all replacement state is ready — now mutate.
        // Remove the old document only after we know the replacement is non-empty.
        if is_reindex {
            self.remove_document(&doc_id);
        }

        // Get or assign internal u32 ID
        let internal_id = self.get_or_assign_internal_id(&doc_id)?;

        // Update inverted index with sorted insertion to maintain doc_id order.
        // WAND requires posting lists sorted by doc_id for binary-search seeks.
        for (term, freq) in &term_freqs {
            let postings = self.inverted_index.entry(term.clone()).or_default();
            let insert_at = postings.partition_point_by_doc_id(internal_id);
            // Clamp to u8::MAX (255) for compact posting storage.
            // BM25's TF saturation means tf>10 is already ~85% of max
            // contribution at k1=1.2, so clamping at 255 has negligible
            // scoring impact. For very long documents (>255 occurrences of
            // a single term), the score will plateau slightly early.
            postings.insert(insert_at, internal_id, (*freq).min(255) as u8);
        }

        // Populate forward index: doc -> list of its terms (for O(terms) removal).
        self.forward_index
            .insert(internal_id, term_freqs.keys().cloned().collect());

        // Update document metadata
        self.doc_lengths.insert(internal_id, doc_length);
        self.set_doc_length_fast(internal_id, doc_length);
        // Saturating add: total_tokens is used only for avgdl; saturating at
        // usize::MAX means avgdl will be imprecise at extreme scale but will
        // not panic or overflow.
        self.total_tokens = self.total_tokens.saturating_add(doc_length);

        // IDF cache auto-invalidates on the next search when it detects
        // that doc_count() has changed. No per-term eviction needed.

        // Block-max metadata is epoch-invalidated (lazy rebuild on next WAND search).
        self.invalidate_block_max_after_mutation();

        Ok(())
    }

    /// Remove a document; returns `true` if found and removed.
    pub fn remove_document(&mut self, doc_id: &str) -> bool {
        // Look up internal ID
        let internal_id = match self.id_to_internal.get(doc_id).copied() {
            Some(id) => id,
            None => return false,
        };

        // Get and remove document length
        let doc_length = match self.doc_lengths.remove(&internal_id) {
            Some(len) => len,
            None => return false,
        };

        // Clear the fast-path vec entries (both usize and f32 mirrors).
        let idx = internal_id as usize;
        if idx < self.doc_lengths_vec.len() {
            self.doc_lengths_vec[idx] = 0;
        }
        if idx < self.doc_lengths_f32.len() {
            self.doc_lengths_f32[idx] = 0.0;
        }

        // Update total tokens
        self.total_tokens = self.total_tokens.saturating_sub(doc_length);

        // Ensure the forward index is populated (rebuilds lazily from the inverted
        // index after deserialization so that removes are always O(|terms_in_doc|)).
        self.ensure_forward_index();

        // Remove from posting lists using the forward index (O(terms_in_doc) not O(|V|)).
        if let Some(terms) = self.forward_index.remove(&internal_id) {
            for term in &terms {
                if let Some(postings) = self.inverted_index.get_mut(term) {
                    let idx = postings.partition_point_by_doc_id(internal_id);
                    if idx < postings.len() && postings.doc_ids[idx] == internal_id {
                        postings.remove(idx);
                    }
                    if postings.is_empty() {
                        self.inverted_index.remove(term);
                    }
                }
            }
        }

        // Remove from ID maps
        self.id_to_internal.remove(doc_id);
        // Note: don't remove from internal_to_id Vec (leaves hole, but u32 IDs are never reused)

        // IDF cache auto-invalidates on the next search when it detects
        // that doc_count() has changed. No per-term eviction needed.

        // Block-max metadata is epoch-invalidated.
        self.invalidate_block_max_after_mutation();

        true
    }
}

#[cfg(test)]
mod forward_index_tests {
    use crate::{Bm25Config, Bm25Index};

    #[test]
    fn test_forward_index_persisted_across_save_load_cycle() {
        let mut index = Bm25Index::default();
        index.index_document("doc1", "quick brown fox").unwrap();
        index.index_document("doc2", "lazy brown dog").unwrap();
        index.index_document("doc3", "quick fox jumps").unwrap();

        let json = serde_json::to_string(&index).unwrap();
        let restored: Bm25Index = serde_json::from_str(&json).unwrap();

        assert!(
            !restored.forward_index.is_empty(),
            "forward_index must be populated after custom deserialization"
        );

        for internal_id in restored.doc_lengths.keys() {
            assert!(
                restored.forward_index.contains_key(internal_id),
                "doc {internal_id} missing from rebuilt forward_index"
            );
        }
    }

    #[test]
    fn test_remove_uses_forward_index_not_full_scan() {
        let mut index = Bm25Index::default();
        let words = [
            "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa",
        ];
        for (i, word) in words.iter().enumerate() {
            index
                .index_document(format!("doc{i}"), &format!("{word} shared_term"))
                .unwrap();
        }

        let json = serde_json::to_string(&index).unwrap();
        let mut restored: Bm25Index = serde_json::from_str(&json).unwrap();

        assert!(
            !restored.forward_index.is_empty(),
            "custom Deserialize must rebuild forward_index immediately"
        );
        for internal_id in restored.doc_lengths.keys() {
            assert!(
                restored.forward_index.contains_key(internal_id),
                "doc {internal_id} missing from rebuilt forward_index after deserialization"
            );
        }

        let removed = restored.remove_document("doc0");
        assert!(
            removed,
            "remove_document must return true for an existing doc"
        );

        assert!(!restored.contains_document("doc0"));
        assert_eq!(
            restored.doc_count(),
            words.len() - 1,
            "doc_count must decrease by exactly one"
        );

        for i in 1..words.len() {
            let doc_id = format!("doc{i}");
            let ok = restored.remove_document(&doc_id);
            assert!(ok, "remove_document must succeed for {doc_id}");
        }
        assert_eq!(restored.doc_count(), 0);
        assert!(
            restored.inverted_index.is_empty(),
            "inverted index must be empty after all removes"
        );
    }

    #[test]
    fn test_search_results_unchanged_after_add_remove_cycle() {
        let mut baseline_index = Bm25Index::try_new(Bm25Config::default()).expect("valid config");
        baseline_index
            .index_document("doc1", "quick brown fox")
            .unwrap();
        baseline_index
            .index_document("doc2", "lazy brown dog")
            .unwrap();
        baseline_index
            .index_document("doc3", "quick fox jumps")
            .unwrap();
        let baseline = baseline_index.search("quick brown fox", 10);

        baseline_index
            .index_document("doc4", "unrelated zebra content")
            .unwrap();
        let json = serde_json::to_string(&baseline_index).unwrap();
        let mut restored: Bm25Index = serde_json::from_str(&json).unwrap();
        restored.ensure_doc_lengths_vec();

        let removed = restored.remove_document("doc4");
        assert!(removed, "doc4 must be removable from the restored index");

        let after = restored.search("quick brown fox", 10);

        assert_eq!(
            baseline.len(),
            after.len(),
            "result count must match the original 3-doc baseline after remove cycle"
        );
        for (base, post) in baseline.iter().zip(after.iter()) {
            assert_eq!(
                base.0, post.0,
                "doc_id ordering must be preserved after remove cycle"
            );
            assert_eq!(
                base.1, post.1,
                "BM25 scores must be identical after remove cycle"
            );
        }
    }
}
