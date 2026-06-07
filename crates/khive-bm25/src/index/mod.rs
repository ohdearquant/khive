//! In-memory BM25 inverted index with WAND acceleration and SIMD scoring.

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

pub(crate) use posting::BlockMaxBlock;
pub(crate) use scoring::{idf_from_doc_freq, Bm25TermScorer};
