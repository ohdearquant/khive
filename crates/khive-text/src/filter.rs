//! Token filters: lowercase, stop words, length constraints, stemming.

use std::collections::HashSet;
use std::sync::LazyLock;

use crate::TokenFilter;

/// Lowercases tokens (Unicode-aware).
#[derive(Debug, Default, Clone)]
pub struct LowercaseFilter;

impl TokenFilter for LowercaseFilter {
    fn apply(&self, token: String) -> Option<String> {
        Some(token.to_lowercase())
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

/// Drops English stop words. Assumes input is already lowercased.
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

/// Snowball stemmer. Only stems ASCII-alphabetic tokens; others pass through.
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
mod tests {
    use super::*;

    #[test]
    fn lowercase_ascii() {
        assert_eq!(LowercaseFilter.apply("HELLO".into()), Some("hello".into()));
    }

    #[test]
    fn lowercase_unicode() {
        assert_eq!(
            LowercaseFilter.apply("Straße".into()),
            Some("straße".into())
        );
    }

    #[test]
    fn min_length_boundary() {
        let f = MinLengthFilter(3);
        assert_eq!(f.apply("ab".into()), None);
        assert_eq!(f.apply("abc".into()), Some("abc".into()));
    }

    #[test]
    fn max_length_boundary() {
        let f = MaxLengthFilter(5);
        assert_eq!(f.apply("hello".into()), Some("hello".into()));
        assert_eq!(f.apply("helloo".into()), None);
    }

    #[test]
    fn max_length_default_is_40() {
        let f = MaxLengthFilter::default();
        assert_eq!(f.0, 40);
    }

    #[test]
    fn stop_word_drops() {
        assert_eq!(StopWordFilter.apply("the".into()), None);
        assert_eq!(StopWordFilter.apply("is".into()), None);
        assert_eq!(
            StopWordFilter.apply("transformer".into()),
            Some("transformer".into())
        );
    }

    #[test]
    fn stop_word_case_sensitive() {
        // StopWordFilter expects lowercased input
        assert_eq!(StopWordFilter.apply("The".into()), Some("The".into()));
    }
}
