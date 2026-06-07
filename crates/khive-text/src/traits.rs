//! Core text analysis traits and shared type aliases.

use std::sync::Arc;

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
