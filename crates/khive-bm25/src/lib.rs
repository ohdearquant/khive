//! BM25 keyword index with Block-Max WAND acceleration.

pub mod error;
pub mod metrics;

mod config;
mod index;
mod tokenizer;

pub use config::Bm25Config;
pub use error::{ErrorKind, Result, RetrievalError};
pub use index::{Bm25Index, Bm25Stats, DocumentId, PostingList, SearchContext};
pub use tokenizer::{tokenize, BoxedTokenizer, SimpleTokenizer, Tokenizer};

pub use khive_score::DeterministicScore;

#[doc(hidden)]
pub use index::DEFAULT_BLOCK_SIZE;
