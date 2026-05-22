//! BM25 (Okapi BM25) keyword index.
//!
//! Provides term frequency-based relevance scoring for keyword search.
//! See ADR-003 for configuration (k1=1.2, b=0.75).
//!
//! # BM25 Formula
//!
//! ```text
//! score(D, Q) = Σ IDF(qi) * (f(qi, D) * (k1 + 1)) / (f(qi, D) + k1 * (1 - b + b * |D|/avgdl))
//!
//! where:
//! - Q = query terms
//! - D = document
//! - f(qi, D) = term frequency of qi in D
//! - |D| = document length
//! - avgdl = average document length
//! - k1 = 1.2 (term saturation)
//! - b = 0.75 (length normalization)
//! ```
//!
//! # Example
//!
//! ```rust
//! use khive_bm25::{Bm25Config, Bm25Index};
//!
//! let mut index = Bm25Index::new(Bm25Config::default());
//!
//! // Index some documents (String / &str auto-convert to DocumentId)
//! index.index_document("doc1", "the quick brown fox").unwrap();
//! index.index_document("doc2", "the lazy dog").unwrap();
//! index.index_document("doc3", "quick brown fox jumps over the lazy dog").unwrap();
//!
//! // Search
//! let results = index.search("quick fox", 10);
//! for (doc_id, score) in results {
//!     println!("{}: {}", doc_id, score);
//! }
//! ```
//!
//! # ID Types and Hybrid Search Bridging
//!
//! [`DocumentId`] is a newtype wrapper around `String` that provides type
//! safety. When performing hybrid search that combines BM25 results with
//! HNSW vector results (which use `EmbeddingId`), see the [`DocumentId`]
//! documentation for bridging strategies.

pub mod error;
pub mod metrics;

mod config;
mod index;
mod tokenizer;

#[cfg(test)]
mod tests;

// Re-export public types
pub use config::Bm25Config;
pub use error::{ErrorKind, Result, RetrievalError};
pub use index::{Bm25Index, Bm25Stats, DocumentId, SearchContext};
pub use tokenizer::{tokenize, BoxedTokenizer, SimpleTokenizer, Tokenizer};
