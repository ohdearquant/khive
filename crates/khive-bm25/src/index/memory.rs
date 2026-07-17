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

        let doc_lengths_f32_size = self.doc_lengths_f32.len() * size_of::<f32>() + 24;

        let index_map_overhead = self.inverted_index.len() * 64;

        let fixed_overhead: usize = 192;

        inverted_index_size
            + block_max_size
            + doc_lengths_size
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
            + forward_index_cost
            + id_map_cost
    }
}
