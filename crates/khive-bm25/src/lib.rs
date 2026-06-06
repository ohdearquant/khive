//! BM25 (Okapi BM25) keyword index.
//!
//! Term frequency-based relevance scoring with Block-Max WAND acceleration.
//! See ADR-003 for configuration and `docs/usage.md` for formula, examples, and ID bridging.

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
pub use index::{Bm25Index, Bm25Stats, DocumentId, PostingList, SearchContext};
pub use tokenizer::{tokenize, BoxedTokenizer, SimpleTokenizer, Tokenizer};

// Re-export score type used in search results so external callers need only
// depend on khive-bm25 and not khive-score directly.
pub use khive_score::DeterministicScore;

// Expose the default block size so integration tests can construct block-boundary
// regression corpora without hard-coding the internal constant.
#[doc(hidden)]
pub use index::DEFAULT_BLOCK_SIZE;
