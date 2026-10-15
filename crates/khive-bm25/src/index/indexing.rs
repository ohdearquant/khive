//! Document indexing operations for BM25 index.

use std::collections::BTreeMap;

use super::{Bm25Index, DocumentId};
use crate::error::{Result, RetrievalError};
use crate::metrics::{self, MetricEvent, MetricValue};

impl Bm25Index {
    /// Index or replace a document; empty replacements preserve existing content.
    ///
    /// New IDs may return [`RetrievalError::BudgetExceeded`]; existing IDs bypass admission.
    /// See `crates/khive-bm25/docs/api/index-lifecycle.md`.
    pub fn index_document(&mut self, doc_id: impl Into<DocumentId>, text: &str) -> Result<()> {
        let start = std::time::Instant::now();

        let result = self.index_document_inner(doc_id, text);

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

    /// Tokenize and validate before applying a replacement.
    fn index_document_inner(&mut self, doc_id: impl Into<DocumentId>, text: &str) -> Result<()> {
        let doc_id: DocumentId = doc_id.into();
        let is_reindex = self.contains_document(&doc_id);

        // Prepare all fallible replacement state before mutating the index.
        let tokens = self.tokenizer.tokenize(text);
        let doc_length = tokens.len();

        if doc_length == 0 {
            return Ok(());
        }

        let mut term_freqs: BTreeMap<String, u32> = BTreeMap::new();
        for token in &tokens {
            *term_freqs.entry(token.clone()).or_insert(0) += 1;
        }

        if !is_reindex {
            if let Some(limit) = self.config.memory_budget {
                let current = self.memory_usage();
                let cost = self.estimate_document_cost(text);
                if current.saturating_add(cost) > limit {
                    return Err(RetrievalError::budget_exceeded(current, cost, limit));
                }
            }
        }

        // Remove old content only after the replacement is known to be usable.
        if is_reindex {
            self.remove_document(&doc_id);
        }

        let internal_id = self.get_or_assign_internal_id(&doc_id)?;

        // WAND requires posting lists sorted by doc_id for binary-search seeks.
        for (term, freq) in &term_freqs {
            let postings = self.inverted_index.entry(term.clone()).or_default();
            let insert_at = postings.partition_point_by_doc_id(internal_id);
            // Compact `u8` TF storage is acceptable because BM25 saturates contribution.
            postings.insert(insert_at, internal_id, (*freq).min(255) as u8);
        }

        self.forward_index
            .insert(internal_id, term_freqs.keys().cloned().collect());

        self.doc_lengths.insert(internal_id, doc_length);
        self.set_doc_length_fast(internal_id, doc_length);
        // Prefer imprecise average length at extreme scale over overflow.
        self.total_tokens = self.total_tokens.saturating_add(doc_length);

        self.invalidate_block_max_after_mutation();

        Ok(())
    }

    /// Remove a document, returning whether it existed; internal IDs are not reused.
    ///
    /// See `crates/khive-bm25/docs/api/index-lifecycle.md`.
    pub fn remove_document(&mut self, doc_id: &str) -> bool {
        let internal_id = match self.id_to_internal.get(doc_id).copied() {
            Some(id) => id,
            None => return false,
        };

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

        self.total_tokens = self.total_tokens.saturating_sub(doc_length);

        // Rebuild deserialized state before the O(terms-in-document) removal.
        self.ensure_forward_index();

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

        self.id_to_internal.remove(doc_id);
        // Note: don't remove from internal_to_id Vec (leaves hole, but u32 IDs are never reused)
        self.invalidate_block_max_after_mutation();

        true
    }
}

#[cfg(test)]
#[path = "indexing_forward_index_tests.rs"]
mod forward_index_tests;
