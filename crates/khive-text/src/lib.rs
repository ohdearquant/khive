//! Text analysis primitives: tokenization, normalization, filtering.

mod traits;
pub mod analyzer;
pub mod filter;
pub mod identifier;
pub mod lang;
pub mod preset;
pub mod tokenizer;

pub use traits::{Analyzer, BoxedAnalyzer, BoxedTokenizer, TokenFilter, Tokenizer};
pub use analyzer::StandardAnalyzer;
pub use lang::{contains_cjk, is_cjk_char, is_meaningful_query, ScriptProfile};
