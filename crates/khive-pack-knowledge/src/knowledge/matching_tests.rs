use super::*;

#[test]
fn tokenize_splits_whitespace_snake_camel() {
    assert_eq!(
        tokenize_field("getString from_the registry"),
        vec!["get", "string", "from", "the", "registry"]
    );
}

#[test]
fn tokenize_lowercases() {
    assert_eq!(
        tokenize_field("Valuation Rings"),
        vec!["valuation", "rings"]
    );
}

#[test]
fn symbol_suffixed_tokens_stay_atomic() {
    let toks = tokenize_field("C++ and C# programming");
    assert!(has_in_tokens(&toks, "c++"));
    assert!(has_in_tokens(&toks, "c#"));
    assert!(!has_in_tokens(&toks, "c"));
}

#[test]
fn matching_is_exact_not_substring() {
    let toks = tokenize_field("factor history torus");
    assert!(!has_in_tokens(&toks, "tor"));
    assert_eq!(count_in_tokens(&toks, "tor"), 0);
    assert!(has_in_tokens(&toks, "factor"));
}

#[test]
fn count_tallies_repeats() {
    let toks = tokenize_field("ring ring ring theory");
    assert_eq!(count_in_tokens(&toks, "ring"), 3);
    assert_eq!(count_in_tokens(&toks, "theory"), 1);
    assert_eq!(count_in_tokens(&toks, "field"), 0);
}

#[test]
fn empty_inputs_are_safe() {
    assert!(tokenize_field("").is_empty());
    assert!(!has_in_tokens(&[], "ring"));
    assert_eq!(count_in_tokens(&[], "ring"), 0);
}
