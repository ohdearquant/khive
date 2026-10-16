use super::*;
use crate::Analyzer;

#[test]
fn standard_english() {
    let a = standard();
    // "over" is not in the BM25 default stop list, so it survives under
    // the parity contract even though khive-text's own StopWordFilter
    // would have dropped it.
    let tokens = a.analyze("The quick brown fox jumps over the lazy dog");
    assert_eq!(
        tokens,
        vec!["quick", "brown", "fox", "jumps", "over", "lazy", "dog"]
    );
}

#[test]
fn standard_drops_stops_but_keeps_short_non_stop_tokens() {
    let a = standard();
    // "am" is not a BM25 stop word and min_length is 1, so it survives.
    let tokens = a.analyze("I am a test");
    assert_eq!(tokens, vec!["am", "test"]);
}

#[test]
fn standard_empty() {
    assert!(standard().analyze("").is_empty());
}

#[test]
fn standard_whitespace_only() {
    assert!(standard().analyze("   ").is_empty());
}

#[test]
fn standard_single_stop_word() {
    assert!(standard().analyze("a").is_empty());
}

#[test]
fn simple_keeps_stop_words() {
    let tokens = simple().analyze("The quick fox");
    assert_eq!(tokens, vec!["the", "quick", "fox"]);
}

#[test]
fn keyword_preserves_phrase() {
    let tokens = keyword().analyze("attention mechanism");
    assert_eq!(tokens, vec!["attention mechanism"]);
}

#[test]
fn keyword_empty() {
    assert!(keyword().analyze("").is_empty());
}

#[test]
fn keyword_preserves_long_whole_input_without_hidden_cap() {
    let input_200 = "a".repeat(200);
    assert_eq!(keyword().analyze(&input_200), vec![input_200.clone()]);

    let input_201 = "A".repeat(201);
    assert_eq!(
        keyword().analyze(&input_201),
        vec![input_201.to_lowercase()]
    );
}

#[test]
fn cjk_mixed_script() {
    let tokens = cjk().analyze("使用LoRA进行");
    assert!(tokens.contains(&"使".to_string()));
    assert!(tokens.contains(&"用".to_string()));
    assert!(tokens.contains(&"lora".to_string()));
    assert!(tokens.contains(&"进".to_string()));
    assert!(tokens.contains(&"行".to_string()));
}

#[test]
fn kg_name_identifier() {
    let tokens = kg_name().analyze("LoRA");
    assert!(tokens.contains(&"lora".to_string()));
    assert!(tokens.contains(&"lo".to_string()));
    assert!(tokens.contains(&"ra".to_string()));
}

#[test]
fn kg_name_hyphenated() {
    let tokens = kg_name().analyze("bert-base-uncased");
    assert!(tokens.contains(&"bert-base-uncased".to_string()));
    assert!(tokens.contains(&"bert".to_string()));
    assert!(tokens.contains(&"base".to_string()));
    assert!(tokens.contains(&"uncased".to_string()));
}

#[test]
fn kg_name_plain_word() {
    let tokens = kg_name().analyze("attention");
    assert_eq!(tokens, vec!["attention"]);
}

#[test]
fn standard_keeps_long_tokens_like_bm25() {
    // BM25's SimpleTokenizer::default() has no max-length filter, so
    // standard() must not drop long tokens either.
    let long = "a".repeat(50);
    let input = format!("hello {long} world");
    let tokens = standard().analyze(&input);
    assert_eq!(tokens, vec!["hello".to_string(), long, "world".to_string()]);
}
