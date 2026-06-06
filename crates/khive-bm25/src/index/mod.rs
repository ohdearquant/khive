//! BM25 inverted index: in-memory inverted index with WAND acceleration and SIMD scoring.
//!
//! Scores are converted to `DeterministicScore` at the API boundary for cross-platform
//! consistency. See `docs/algorithm.md` for BM25 properties, floating-point design
//! rationale, WAND block-max details, IDF cache design, and thread-safety trade-offs.

mod core;
mod document_id;
mod indexing;
mod memory;
mod posting;
mod scoring;
mod search;

pub use core::{Bm25Index, DEFAULT_BLOCK_SIZE};
pub use document_id::DocumentId;
pub use posting::PostingList;
pub use scoring::Bm25Stats;
pub use search::SearchContext;

// Internal re-exports for submodule access.
pub(crate) use posting::BlockMaxBlock;
pub(crate) use scoring::{idf_from_doc_freq, Bm25TermScorer};
