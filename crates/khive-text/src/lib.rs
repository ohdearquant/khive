//! Text analysis primitives for khive: tokenization, normalization, filtering.
//!
//! Three composable traits: Tokenizer (split) -> TokenFilter (transform/drop) -> Analyzer (pipeline).

use std::sync::Arc;

pub mod analyzer;
pub mod filter;
pub mod identifier;
pub mod lang;
pub mod preset;
pub mod tokenizer;

pub use analyzer::StandardAnalyzer;
pub use lang::{contains_cjk, is_cjk_char, is_meaningful_query, ScriptProfile};

/// Splits a string into raw tokens. Must be deterministic and stateless.
pub trait Tokenizer: Send + Sync {
    fn tokenize(&self, text: &str) -> Vec<String>;
}

/// Transforms or drops a single token. Returns None to drop.
pub trait TokenFilter: Send + Sync {
    fn apply(&self, token: String) -> Option<String>;
}

/// Full analysis pipeline: tokenize + filter chain.
pub trait Analyzer: Send + Sync {
    fn analyze(&self, text: &str) -> Vec<String>;
}

pub type BoxedAnalyzer = Arc<dyn Analyzer>;
pub type BoxedTokenizer = Arc<dyn Tokenizer>;
