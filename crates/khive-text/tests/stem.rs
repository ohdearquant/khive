//! Behavior tests for the `stem` feature-gated SnowballStemmer.

#![cfg(feature = "stem")]

use khive_text::filter::SnowballStemmer;
use khive_text::TokenFilter;

#[test]
fn english_stemmer_reduces_common_suffixes() {
    let s = SnowballStemmer::english();
    assert_eq!(s.apply("running".to_string()), Some("run".to_string()));
    assert_eq!(
        s.apply("connections".to_string()),
        Some("connect".to_string())
    );
    assert_eq!(s.apply("easily".to_string()), Some("easili".to_string()));
}

#[test]
fn stemmer_passes_through_non_ascii_tokens() {
    let s = SnowballStemmer::english();
    // Non-ASCII tokens are returned unchanged
    assert_eq!(
        s.apply("caf\u{00e9}".to_string()),
        Some("caf\u{00e9}".to_string())
    );
    assert_eq!(
        s.apply("\u{4f60}\u{597d}".to_string()),
        Some("\u{4f60}\u{597d}".to_string())
    );
}

#[test]
fn stemmer_passes_through_mixed_ascii_nonalpha() {
    let s = SnowballStemmer::english();
    // Tokens with digits or punctuation are not purely ASCII-alphabetic
    assert_eq!(s.apply("v2".to_string()), Some("v2".to_string()));
    assert_eq!(
        s.apply("fine-tuning".to_string()),
        Some("fine-tuning".to_string())
    );
}

#[test]
fn stemmer_handles_already_stemmed() {
    let s = SnowballStemmer::english();
    // "run" is already a stem
    assert_eq!(s.apply("run".to_string()), Some("run".to_string()));
}

#[test]
fn stemmer_handles_empty_token() {
    let s = SnowballStemmer::english();
    // Empty string has no ASCII-alphabetic chars, passes through
    assert_eq!(s.apply(String::new()), Some(String::new()));
}
