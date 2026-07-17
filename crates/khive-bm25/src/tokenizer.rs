//! Pluggable tokenizer trait with a simple English whitespace default.

use std::collections::HashSet;
use std::sync::{Arc, LazyLock};

/// Deterministic, thread-safe term extraction for BM25 indexing.
///
/// See `crates/khive-bm25/docs/api/tokenizer.md`.
pub trait Tokenizer: Send + Sync {
    /// Tokenize text into terms. Returns empty vec for empty input.
    fn tokenize(&self, text: &str) -> Vec<String>;
}

/// Box type for tokenizers (enables dynamic dispatch).
pub type BoxedTokenizer = Arc<dyn Tokenizer>;

/// English stop words filtered from BM25 postings to reduce index size.
static STOP_WORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        "a", "an", "and", "are", "as", "at", "be", "been", "being", "but", "by", "can", "did",
        "do", "does", "doing", "done", "for", "from", "had", "has", "have", "having", "he", "her",
        "here", "hers", "him", "his", "how", "i", "if", "in", "into", "is", "it", "its", "just",
        "may", "me", "might", "my", "no", "nor", "not", "of", "on", "or", "our", "out", "own",
        "say", "she", "should", "so", "some", "such", "than", "that", "the", "their", "them",
        "then", "there", "these", "they", "this", "those", "through", "to", "too", "up", "us",
        "very", "was", "we", "were", "what", "when", "where", "which", "while", "who", "whom",
        "why", "will", "with", "would", "you", "your",
    ])
});

/// Configurable whitespace tokenizer with punctuation, case, length, and stop-word processing.
///
/// See `crates/khive-bm25/docs/api/tokenizer.md`.
#[derive(Debug, Clone)]
pub struct SimpleTokenizer {
    /// Whether to lowercase tokens.
    pub lowercase: bool,
    /// Minimum token length (tokens shorter than this are filtered out).
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
    /// Create a new SimpleTokenizer with specified options.
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
        // Average English word length gives a useful allocation estimate.
        let estimated_tokens = text.len() / 6 + 1;
        let mut result = Vec::with_capacity(estimated_tokens.min(32));

        for word in text.split_whitespace() {
            let trimmed = word.trim_matches(|c: char| c.is_ascii_punctuation());

            if trimmed.len() < self.min_length {
                continue;
            }

            // Avoid Unicode lowercase allocation for the common ASCII path.
            let token = if self.lowercase {
                if trimmed.is_ascii() {
                    let mut s = String::with_capacity(trimmed.len());
                    for &byte in trimmed.as_bytes() {
                        s.push(byte.to_ascii_lowercase() as char);
                    }
                    s
                } else {
                    trimmed.to_lowercase()
                }
            } else {
                trimmed.to_string()
            };

            if self.filter_stop_words && STOP_WORDS.contains(token.as_str()) {
                continue;
            }

            result.push(token);
        }

        result
    }
}

/// Tokenize text with `SimpleTokenizer::default()`; empty input returns no terms.
pub fn tokenize(text: &str) -> Vec<String> {
    SimpleTokenizer::default().tokenize(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tokenize_filters_stop_words() {
        let tokens = tokenize("The Quick, Brown FOX!");
        // "the" is a stop word, filtered out
        assert_eq!(tokens, vec!["quick", "brown", "fox"]);
    }

    #[test]
    fn test_tokenize_empty() {
        let tokens = tokenize("");
        assert!(tokens.is_empty());

        let tokens = tokenize("   ");
        assert!(tokens.is_empty());
    }

    #[test]
    fn test_tokenize_punctuation_only() {
        let tokens = tokenize("... !!! ???");
        assert!(tokens.is_empty());
    }

    #[test]
    fn test_tokenize_case_insensitive() {
        let tokens = tokenize("HELLO World hElLo");
        assert_eq!(tokens, vec!["hello", "world", "hello"]);
    }

    #[test]
    fn test_tokenize_stop_words_removed() {
        // "how", "are", "you" are stop words
        let tokens = tokenize("Hello, World! How are you?");
        assert_eq!(tokens, vec!["hello", "world"]);
    }

    #[test]
    fn test_tokenize_multiple_spaces() {
        let tokens = tokenize("hello    world");
        assert_eq!(tokens, vec!["hello", "world"]);
    }

    #[test]
    fn test_simple_tokenizer_no_lowercase() {
        let tokenizer = SimpleTokenizer::new(false, 1);
        // "Hello" and "World" are not stop words (case-sensitive, and stop words are lowercase)
        let tokens = tokenizer.tokenize("Hello World");
        assert_eq!(tokens, vec!["Hello", "World"]);
    }

    #[test]
    fn test_simple_tokenizer_min_length() {
        let tokenizer = SimpleTokenizer::new(true, 3);
        let tokens = tokenizer.tokenize("I am a cat");
        // "I", "am", "a" filtered by min_length; also stop words
        assert_eq!(tokens, vec!["cat"]);
    }

    #[test]
    fn test_trait_object_usage() {
        let tokenizer: BoxedTokenizer = Arc::new(SimpleTokenizer::default());
        let tokens = tokenizer.tokenize("hello world");
        assert_eq!(tokens, vec!["hello", "world"]);
    }

    #[test]
    fn test_stop_words_disabled() {
        let tokenizer = SimpleTokenizer {
            filter_stop_words: false,
            ..Default::default()
        };
        let tokens = tokenizer.tokenize("The Quick, Brown FOX!");
        assert_eq!(tokens, vec!["the", "quick", "brown", "fox"]);
    }

    #[test]
    fn test_all_stop_words_returns_empty() {
        let tokens = tokenize("the and or but");
        assert!(tokens.is_empty());
    }
}
