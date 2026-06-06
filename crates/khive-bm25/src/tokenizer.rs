//! Tokenization for BM25.
//!
//! Pluggable tokenizer trait with a simple English whitespace default.
//! See `docs/tokenization.md` for deferred features (CJK, stemming, stop words).

use std::collections::HashSet;
use std::sync::{Arc, LazyLock};

/// Tokenizer trait for extensible text tokenization.
///
/// Implement this trait to provide custom tokenization for BM25 search.
/// This enables:
/// - Language-specific tokenization (CJK, Arabic, etc.)
/// - Stemming/lemmatization
/// - Stop word removal
/// - N-gram support
pub trait Tokenizer: Send + Sync {
    /// Tokenize the input text into a list of tokens.
    ///
    /// # Arguments
    ///
    /// * `text` - Input text to tokenize
    ///
    /// # Returns
    ///
    /// Vector of tokens (strings). Empty vector for empty input.
    fn tokenize(&self, text: &str) -> Vec<String>;
}

/// Box type for tokenizers (enables dynamic dispatch).
pub type BoxedTokenizer = Arc<dyn Tokenizer>;

/// English stop words — high-frequency terms that add noise to BM25 postings
/// without improving retrieval quality. Removing these reduces BM25 memory by
/// ~170 MB at 15K docs (each stop word creates N postings × 64 bytes).
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

/// Simple whitespace tokenizer with optional lowercase, minimum length,
/// and stop-word filtering.
///
/// This is the default tokenizer suitable for English text.
/// For production use with non-English text, consider implementing
/// a custom tokenizer with proper segmentation for your language.
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
    ///
    /// # Arguments
    ///
    /// * `lowercase` - Whether to convert tokens to lowercase
    /// * `min_length` - Minimum token length (shorter tokens are filtered out)
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
        // Fast path: estimate capacity to avoid re-allocations.
        // Average English word ~5 chars + 1 space, so text.len()/6 is a reasonable estimate.
        let estimated_tokens = text.len() / 6 + 1;
        let mut result = Vec::with_capacity(estimated_tokens.min(32));

        for word in text.split_whitespace() {
            // Remove leading/trailing punctuation
            let trimmed = word.trim_matches(|c: char| c.is_ascii_punctuation());

            if trimmed.len() < self.min_length {
                continue;
            }

            // Fast ASCII lowercase check: if all bytes are ASCII, lowercase in-place
            // to avoid the overhead of `str::to_lowercase()` (which handles Unicode).
            let token = if self.lowercase {
                if trimmed.is_ascii() {
                    // Fast path: ASCII-only, lowercase via byte manipulation
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

/// Convenience function for simple tokenization (backwards compatibility).
///
/// Uses the default SimpleTokenizer configuration.
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
