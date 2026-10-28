//! Token filters: lowercase, stop words, length constraints, stemming.

use std::collections::HashSet;
use std::sync::LazyLock;

use crate::TokenFilter;

/// Lowercases tokens (Unicode-aware).
#[derive(Debug, Default, Clone)]
pub struct LowercaseFilter;

impl TokenFilter for LowercaseFilter {
    fn apply(&self, mut token: String) -> Option<String> {
        if token.is_ascii() {
            token.make_ascii_lowercase();
            Some(token)
        } else {
            Some(token.to_lowercase())
        }
    }
}

/// Drops tokens shorter than `min` characters.
#[derive(Debug, Clone)]
pub struct MinLengthFilter(pub usize);

impl TokenFilter for MinLengthFilter {
    fn apply(&self, token: String) -> Option<String> {
        if token.chars().count() >= self.0 {
            Some(token)
        } else {
            None
        }
    }
}

/// Drops tokens longer than `max` characters.
#[derive(Debug, Clone)]
pub struct MaxLengthFilter(pub usize);

impl Default for MaxLengthFilter {
    fn default() -> Self {
        Self(40)
    }
}

impl TokenFilter for MaxLengthFilter {
    fn apply(&self, token: String) -> Option<String> {
        if token.chars().count() <= self.0 {
            Some(token)
        } else {
            None
        }
    }
}

/// Drops the general English stop-word set; input must already be lowercase.
///
/// See `crates/khive-text/docs/api/filters.md`.
#[derive(Debug, Default, Clone)]
pub struct StopWordFilter;

static EN_STOP_WORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "a",
        "about",
        "above",
        "after",
        "again",
        "against",
        "all",
        "am",
        "an",
        "and",
        "any",
        "are",
        "as",
        "at",
        "be",
        "because",
        "been",
        "before",
        "being",
        "below",
        "between",
        "both",
        "but",
        "by",
        "can",
        "could",
        "did",
        "do",
        "does",
        "doing",
        "don't",
        "down",
        "during",
        "each",
        "few",
        "for",
        "from",
        "further",
        "get",
        "got",
        "had",
        "has",
        "have",
        "having",
        "he",
        "her",
        "here",
        "hers",
        "herself",
        "him",
        "himself",
        "his",
        "how",
        "i",
        "if",
        "in",
        "into",
        "is",
        "it",
        "its",
        "itself",
        "just",
        "me",
        "more",
        "most",
        "my",
        "myself",
        "no",
        "nor",
        "not",
        "now",
        "of",
        "off",
        "on",
        "once",
        "only",
        "or",
        "other",
        "our",
        "ours",
        "ourselves",
        "out",
        "over",
        "own",
        "same",
        "she",
        "should",
        "so",
        "some",
        "such",
        "than",
        "that",
        "the",
        "their",
        "theirs",
        "them",
        "themselves",
        "then",
        "there",
        "these",
        "they",
        "this",
        "those",
        "through",
        "to",
        "too",
        "under",
        "until",
        "up",
        "us",
        "very",
        "was",
        "we",
        "were",
        "what",
        "when",
        "where",
        "which",
        "while",
        "who",
        "whom",
        "why",
        "will",
        "with",
        "would",
        "you",
        "your",
        "yours",
        "yourself",
        "yourselves",
    ]
    .into_iter()
    .collect()
});

impl TokenFilter for StopWordFilter {
    fn apply(&self, token: String) -> Option<String> {
        if EN_STOP_WORDS.contains(token.as_str()) {
            None
        } else {
            Some(token)
        }
    }
}

/// Drops the same stop words as `khive-bm25`'s `SimpleTokenizer::default()`.
/// Scoped to `preset::standard()` so the public [`StopWordFilter`] stop list
/// is unaffected. Assumes input is already lowercased.
#[derive(Debug, Default, Clone)]
pub struct Bm25StopWordFilter;

static BM25_STOP_WORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "a", "an", "and", "are", "as", "at", "be", "been", "being", "but", "by", "can", "did",
        "do", "does", "doing", "done", "for", "from", "had", "has", "have", "having", "he", "her",
        "here", "hers", "him", "his", "how", "i", "if", "in", "into", "is", "it", "its", "just",
        "may", "me", "might", "my", "no", "nor", "not", "of", "on", "or", "our", "out", "own",
        "say", "she", "should", "so", "some", "such", "than", "that", "the", "their", "them",
        "then", "there", "these", "they", "this", "those", "through", "to", "too", "up", "us",
        "very", "was", "we", "were", "what", "when", "where", "which", "while", "who", "whom",
        "why", "will", "with", "would", "you", "your",
    ]
    .into_iter()
    .collect()
});

impl TokenFilter for Bm25StopWordFilter {
    fn apply(&self, token: String) -> Option<String> {
        if BM25_STOP_WORDS.contains(token.as_str()) {
            None
        } else {
            Some(token)
        }
    }
}

/// Snowball stemmer for ASCII-alphabetic tokens; all other tokens pass through.
///
/// See `crates/khive-text/docs/api/filters.md`.
#[cfg(feature = "stem")]
pub struct SnowballStemmer(rust_stemmers::Stemmer);

#[cfg(feature = "stem")]
impl SnowballStemmer {
    /// Creates an English Snowball stemmer using the English algorithm.
    pub fn english() -> Self {
        Self(rust_stemmers::Stemmer::create(
            rust_stemmers::Algorithm::English,
        ))
    }

    /// Creates a Snowball stemmer for the specified `rust_stemmers::Algorithm`.
    pub fn for_algorithm(algo: rust_stemmers::Algorithm) -> Self {
        Self(rust_stemmers::Stemmer::create(algo))
    }
}

#[cfg(feature = "stem")]
impl TokenFilter for SnowballStemmer {
    fn apply(&self, token: String) -> Option<String> {
        if token.chars().all(|c| c.is_ascii_alphabetic()) {
            Some(self.0.stem(&token).into_owned())
        } else {
            Some(token)
        }
    }
}

#[cfg(test)]
#[path = "filter/tests.rs"]
mod tests;
