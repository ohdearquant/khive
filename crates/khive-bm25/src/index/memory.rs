//! Memory budget operations for BM25 index.

use std::collections::BTreeMap;
use std::mem::size_of;

use super::{BlockMaxBlock, Bm25Index};

/// Bytes stored per posting across the two structure-of-arrays vectors.
const BYTES_PER_POSTING: usize = size_of::<u32>() + size_of::<u8>();

impl Bm25Index {
    /// Return the optional admission budget in bytes.
    pub fn memory_budget(&self) -> Option<usize> {
        self.config.memory_budget
    }

    /// Set or clear the admission budget; existing documents are not evicted.
    pub fn set_memory_budget(&mut self, budget: Option<usize>) {
        self.config.memory_budget = budget;
    }

    /// Estimate owned index memory, excluding the disposable IDF cache.
    ///
    /// See `crates/khive-bm25/docs/api/memory-budget.md` for accounting assumptions.
    pub fn memory_usage(&self) -> usize {
        let mut inverted_index_size: usize = 0;
        let mut block_max_size: usize = 0;

        for (term, postings) in &self.inverted_index {
            inverted_index_size += 24 + term.len();
            inverted_index_size += 48 + postings.len() * BYTES_PER_POSTING;

            let block_size = self.block_size.max(1);
            let block_count = postings.len().div_ceil(block_size);
            block_max_size += 24 + block_count * size_of::<BlockMaxBlock>();
            block_max_size += 32;
        }

        let doc_lengths_size = self.doc_lengths.len() * (4 + size_of::<usize>() + 32);

        let mut id_map_size: usize = 0;
        for doc_id in self.id_to_internal.keys() {
            id_map_size += 24 + doc_id.len() + 4 + 32;
        }
        id_map_size += 24;
        for doc_id in &self.internal_to_id {
            // Approximate each `Arc<str>` allocation as control block plus string bytes.
            id_map_size += 16 + doc_id.len();
        }

        let mut forward_index_size: usize = self.forward_index.len() * (4 + 24 + 32);
        for terms in self.forward_index.values() {
            for term in terms {
                forward_index_size += 24 + term.len();
            }
        }

        let doc_lengths_vec_size = self.doc_lengths_vec.len() * size_of::<usize>() + 24;
        let doc_lengths_f32_size = self.doc_lengths_f32.len() * size_of::<f32>() + 24;

        let index_map_overhead = self.inverted_index.len() * 64;

        let fixed_overhead: usize = 192;

        inverted_index_size
            + block_max_size
            + doc_lengths_size
            + doc_lengths_vec_size
            + doc_lengths_f32_size
            + forward_index_size
            + id_map_size
            + index_map_overhead
            + fixed_overhead
    }

    /// Estimate the incremental cost of indexing `text` as a new document.
    ///
    /// See `crates/khive-bm25/docs/api/memory-budget.md` for approximation details.
    pub fn estimate_document_cost(&self, text: &str) -> usize {
        let tokens = self.tokenizer.tokenize(text);
        if tokens.is_empty() {
            return 0;
        }

        let mut unique_terms: BTreeMap<&str, u32> = BTreeMap::new();
        for token in &tokens {
            *unique_terms.entry(token.as_str()).or_insert(0) += 1;
        }

        let postings_cost: usize = unique_terms.len() * BYTES_PER_POSTING;

        let new_term_cost: usize = unique_terms
            .keys()
            .filter(|term| !self.inverted_index.contains_key(**term))
            .map(|term| {
                let postings_entry = 24 + term.len() + 48 + 64;
                let block_entry = 24 + size_of::<BlockMaxBlock>() + 32;
                postings_entry + block_entry
            })
            .sum();

        let additional_block_cost: usize = unique_terms
            .keys()
            .filter_map(|term| {
                self.inverted_index
                    .get(*term)
                    .map(|postings| postings.len())
            })
            .map(|old_len| {
                let block_size = self.block_size.max(1);
                let before_blocks = old_len.div_ceil(block_size);
                let after_blocks = (old_len + 1).div_ceil(block_size);
                if after_blocks > before_blocks {
                    size_of::<BlockMaxBlock>()
                } else {
                    0
                }
            })
            .sum();

        let doc_entry_cost: usize = 4 + size_of::<usize>() + 32;

        // The next insert assigns `next_internal_id`, resizing both O(1) doc-length mirrors up to
        // that index. After removals followed by deserialization the mirrors are rebuilt only
        // through the highest live id, so one insert can grow them across the whole gap, not by a
        // single slot.
        let mirror_slots =
            (self.next_internal_id as usize + 1).saturating_sub(self.doc_lengths_vec.len());
        let doc_length_vectors_cost: usize = mirror_slots * (size_of::<usize>() + size_of::<f32>());

        let forward_index_cost: usize = 4
            + 24
            + 32
            + unique_terms
                .keys()
                .map(|term| 24 + term.len())
                .sum::<usize>();

        // No ID is available here, so model a UUID-like external ID.
        let avg_doc_id_len: usize = 36;
        let id_map_cost: usize = (24 + avg_doc_id_len + 4 + 32) // id_to_internal entry
            + (24 + avg_doc_id_len); // internal_to_id slot

        postings_cost
            + new_term_cost
            + additional_block_cost
            + doc_entry_cost
            + doc_length_vectors_cost
            + forward_index_cost
            + id_map_cost
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_usage_accounts_for_doc_lengths_vec_mirror() {
        let mut index = Bm25Index::default();
        let before = index.memory_usage();

        // Grow both O(1) doc-length vector mirrors by one slot, bypassing indexing so no
        // other accounted structure (postings, terms, id maps) changes.
        index.set_doc_length_fast(0, 42);

        let after = index.memory_usage();
        let delta = after - before;

        // Growing each mirror by exactly one slot changes memory_usage by exactly one usize plus
        // one f32; asserting equality keeps the test diagnostic if either term is dropped.
        assert_eq!(
            delta,
            size_of::<usize>() + size_of::<f32>(),
            "memory_usage delta must equal one usize + one f32 doc-length mirror slot"
        );
    }

    #[test]
    fn test_estimate_document_cost_scales_mirror_with_id_gap() {
        // Two indexes identical except for the gap between `next_internal_id` and the rebuilt
        // mirror length, so estimate_document_cost differs ONLY in its doc-length mirror term. This
        // isolates the gap accounting from the estimate's other heuristics.
        let text = "quick brown fox";

        // Contiguous: the next insert resizes the mirrors by a single slot.
        let mut contiguous = Bm25Index::default();
        contiguous.doc_lengths.insert(0, 5);
        contiguous.next_internal_id = 1;
        contiguous.ensure_doc_lengths_vec();
        assert_eq!(contiguous.doc_lengths_vec.len(), 1);
        let contiguous_cost = contiguous.estimate_document_cost(text);

        // Gapped: ids 1..=3 were removed before deserialization, so the next id (4) sits three
        // slots past the rebuilt mirror length and one insert resizes across the whole gap.
        let mut gapped = Bm25Index::default();
        gapped.doc_lengths.insert(0, 5);
        gapped.next_internal_id = 4;
        gapped.ensure_doc_lengths_vec();
        assert_eq!(gapped.doc_lengths_vec.len(), 1);
        let gapped_cost = gapped.estimate_document_cost(text);

        // The gap adds exactly three extra mirror slots (one usize + one f32 each); a per-document
        // "one slot" estimate would report no difference and silently under-admit.
        assert_eq!(
            gapped_cost - contiguous_cost,
            3 * (size_of::<usize>() + size_of::<f32>()),
            "estimate must charge the extra doc-length mirror slots resized across an id gap"
        );
    }
}
