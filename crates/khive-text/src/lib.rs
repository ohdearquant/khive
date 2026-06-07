//! Text analysis primitives: tokenization, normalization, filtering.

pub mod analyzer;
pub mod filter;
pub mod identifier;
pub mod lang;
pub mod preset;
pub mod tokenizer;
mod traits;

pub use analyzer::StandardAnalyzer;
pub use lang::{contains_cjk, is_cjk_char, is_meaningful_query, ScriptProfile};
pub use traits::{Analyzer, BoxedAnalyzer, BoxedTokenizer, TokenFilter, Tokenizer};
