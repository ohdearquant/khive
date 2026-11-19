//! Pluggable tokenizer trait with a simple English whitespace default.

use khive_text::filter::{Bm25StopWordFilter, LowercaseFilter, MinLengthFilter};
use khive_text::tokenizer::WhitespaceTokenizer;
use khive_text::TokenFilter;

pub use khive_text::{BoxedTokenizer, Tokenizer};

/// Configurable whitespace tokenizer with punctuation, case, length, and stop-word processing.
///
/// See `crates/khive-bm25/docs/api/tokenizer.md`.
#[derive(Debug, Clone)]
pub struct SimpleTokenizer {
    /// Whether to lowercase tokens.
    pub lowercase: bool,
    /// Minimum Unicode-scalar token length after punctuation trimming.
    ///
    /// Zero disables the length threshold, but empty normalized tokens are
    /// always discarded.
    pub min_length: usize,
    /// Whether to filter out English stop words.
    pub filter_stop_words: bool,
}

impl Default for SimpleTokenizer {
    fn default() -> Self {
        Self {
            lowercase: true,
            min_length: 1,
            filter_stop_words: true,
        }
    }
}

impl SimpleTokenizer {
    /// Create a new `SimpleTokenizer` with the specified options.
    ///
    /// `min_length = 0` is supported and retains every non-empty normalized
    /// token; punctuation-only input still produces no terms.
    pub fn new(lowercase: bool, min_length: usize) -> Self {
        Self {
            lowercase,
            min_length,
            filter_stop_words: true,
        }
    }
}

impl Tokenizer for SimpleTokenizer {
    fn tokenize(&self, text: &str) -> Vec<String> {
        WhitespaceTokenizer
            .tokenize(text)
            .into_iter()
            .filter_map(|token| MinLengthFilter(self.min_length).apply(token))
            .filter_map(|token| {
                if self.lowercase {
                    LowercaseFilter.apply(token)
                } else {
                    Some(token)
                }
            })
            .filter_map(|token| {
                if self.filter_stop_words {
                    Bm25StopWordFilter.apply(token)
                } else {
                    Some(token)
                }
            })
            .collect()
    }
}

/// Tokenize text with `SimpleTokenizer::default()`; empty input returns no terms.
pub fn tokenize(text: &str) -> Vec<String> {
    SimpleTokenizer::default().tokenize(text)
}

#[cfg(test)]
#[path = "tokenizer_tests.rs"]
mod tests;
