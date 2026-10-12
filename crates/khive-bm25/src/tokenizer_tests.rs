use std::sync::Arc;

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
fn test_simple_tokenizer_min_length_counts_unicode_characters() {
    let tokenizer = SimpleTokenizer::new(true, 2);
    assert_eq!(tokenizer.tokenize("你 rust"), vec!["rust"]);
}

#[test]
fn test_simple_tokenizer_zero_min_length_never_emits_empty_terms() {
    let tokenizer = SimpleTokenizer::new(true, 0);

    assert!(tokenizer.tokenize("!!!").is_empty());
    assert_eq!(tokenizer.tokenize("... alpha ???"), vec!["alpha"]);
}

#[test]
fn test_default_tokenizer_matches_shared_standard_analyzer() {
    use khive_text::{preset, Analyzer};

    let tokenizer = SimpleTokenizer::default();
    let analyzer = preset::standard();
    let cases = [
        "may x",
        "done say might",
        "against",
        &"a".repeat(41),
        "The quick brown fox jumps over the lazy dog",
    ];
    for input in cases {
        assert_eq!(
            tokenizer.tokenize(input),
            analyzer.analyze(input),
            "mismatch for input: {input:?}"
        );
    }
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
