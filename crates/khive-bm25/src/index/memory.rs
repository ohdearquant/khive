//! Memory budget operations for BM25 index.

use std::collections::BTreeMap;
use std::mem::size_of;

use super::{BlockMaxBlock, Bm25Index};

/// Bytes per posting in the SoA layout: u32 doc_id (4) + u8 term_freq (1) = 5.
/// No alignment padding waste (separate Vecs).
const BYTES_PER_POSTING: usize = size_of::<u32>() + size_of::<u8>();

impl Bm25Index {
    /// Get the configured memory budget, if any.
    pub fn memory_budget(&self) -> Option<usize> {
        self.config.memory_budget
    }

    /// Set or clear the memory budget at runtime.
    pub fn set_memory_budget(&mut self, budget: Option<usize>) {
        self.config.memory_budget = budget;
    }

    /// Estimate the index memory usage in bytes (approximation).
    pub fn memory_usage(&self) -> usize {
        let mut inverted_index_size: usize = 0;
        let mut block_max_size: usize = 0;

        for (term, postings) in &self.inverted_index {
            // String key: heap overhead (24) + string data
            inverted_index_size += 24 + term.len();
            // SoA PostingList: two Vec overheads (24 each) +
            // doc_ids (n * 4 bytes) + term_freqs (n * 1 byte) = 5 bytes/posting
            inverted_index_size += 48 + postings.len() * BYTES_PER_POSTING;

            // Block-max metadata sidecar: one Vec<BlockMaxBlock> per term
            let block_size = self.block_size.max(1);
            let block_count = postings.len().div_ceil(block_size);
            block_max_size += 24 + block_count * size_of::<BlockMaxBlock>();
            // HashMap entry overhead for per_term map
            block_max_size += 32;
        }

        // doc_lengths: HashMap<u32, usize>
        // Each entry: u32 key (4) + usize value (8) + HashMap bucket overhead (~32)
        let doc_lengths_size = self.doc_lengths.len() * (4 + size_of::<usize>() + 32);

        // ID mapping tables:
        // id_to_internal: HashMap<DocumentId, u32> -- DocumentId(24 + data) + u32(4) + bucket(32)
        let mut id_map_size: usize = 0;
        for doc_id in self.id_to_internal.keys() {
            id_map_size += 24 + doc_id.len() + 4 + 32;
        }
        // internal_to_id: Vec<Arc<str>> -- vec overhead (24) + each Arc<str> fat-ptr (16) + data
        id_map_size += 24;
        for doc_id in &self.internal_to_id {
            // Arc<str> heap: 16-byte header + string data. Fat-ptr on stack is 16 bytes
            // but we count heap cost; the refcount block is approximated as 16 bytes.
            id_map_size += 16 + doc_id.len();
        }

        // IDF cache: not counted towards budget (it's a cache, can be cleared)

        // Forward index: HashMap<u32, Vec<String>>
        // Each entry: u32 key (4) + Vec overhead (24) + bucket (~32) + string data
        let mut forward_index_size: usize = self.forward_index.len() * (4 + 24 + 32);
        for terms in self.forward_index.values() {
            for term in terms {
                // Each String: 24 bytes overhead + string data
                forward_index_size += 24 + term.len();
            }
        }

        // doc_lengths_f32: Vec<f32> for SIMD batch scoring
        let doc_lengths_f32_size = self.doc_lengths_f32.len() * size_of::<f32>() + 24;

        // HashMap overhead for inverted_index itself
        let index_map_overhead = self.inverted_index.len() * 64;

        // Fixed overhead: config + tokenizer Arc + total_tokens + RwLocks + epoch + block_size
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

    /// Estimate the memory cost of indexing a new document.
    pub fn estimate_document_cost(&self, text: &str) -> usize {
        let tokens = self.tokenizer.tokenize(text);
        if tokens.is_empty() {
            return 0;
        }

        let mut unique_terms: BTreeMap<&str, u32> = BTreeMap::new();
        for token in &tokens {
            *unique_terms.entry(token.as_str()).or_insert(0) += 1;
        }

        // Cost per unique term in SoA layout: u32 (4) + u8 (1) = 5 bytes
        let postings_cost: usize = unique_terms.len() * BYTES_PER_POSTING;

        // New terms that don't exist yet get String key + PostingList overhead + block-max entry
        let new_term_cost: usize = unique_terms
            .keys()
            .filter(|term| !self.inverted_index.contains_key(**term))
            .map(|term| {
                // String key (24 + len) + PostingList overhead (48 = 2 Vecs) + HashMap entry (64)
                let postings_entry = 24 + term.len() + 48 + 64;
                let block_entry = 24 + size_of::<BlockMaxBlock>() + 32;
                postings_entry + block_entry
            })
            .sum();

        // Existing terms may gain an additional block if the posting list crosses a block boundary
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

        // doc_lengths entry: u32(4) + usize(8) + HashMap bucket(32) = 44
        let doc_entry_cost: usize = 4 + size_of::<usize>() + 32;

        // Forward index entry: u32 key (4) + Vec overhead (24) + bucket (32)
        // + each term String (24 + len)
        let forward_index_cost: usize = 4
            + 24
            + 32
            + unique_terms
                .keys()
                .map(|term| 24 + term.len())
                .sum::<usize>();

        // ID mapping cost: DocumentId in both maps + u32 key
        // Assume average doc_id is ~36 bytes (UUID string)
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
