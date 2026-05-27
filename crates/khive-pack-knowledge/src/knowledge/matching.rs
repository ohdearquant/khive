//! Token-aware word-boundary matching for knowledge corpus retrieval.
//!
//! Splits text into lowercase tokens on whitespace, underscores, and camelCase
//! boundaries. Matching is exact token equality — searching `tor` does NOT match
//! inside `factor` or `history`. This keeps precision high for TF-IDF scoring.

fn split_compound(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for snake_part in s.split('_') {
        if snake_part.is_empty() {
            continue;
        }
        split_camel(snake_part, &mut tokens);
    }
    tokens
}

fn split_camel(s: &str, out: &mut Vec<String>) {
    if s.is_empty() {
        return;
    }
    let bytes = s.as_bytes();
    let mut start = 0;
    for i in 1..bytes.len() {
        if bytes[i - 1].is_ascii_lowercase() && bytes[i].is_ascii_uppercase() {
            let token = s[start..i].to_lowercase();
            if !token.is_empty() {
                out.push(token);
            }
            start = i;
        }
    }
    let remainder = s[start..].to_lowercase();
    if !remainder.is_empty() {
        out.push(remainder);
    }
}

/// Tokenize a text field into lowercase compound-split tokens.
pub(crate) fn tokenize_field(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for ws_token in text.split_whitespace() {
        tokens.extend(split_compound(ws_token));
    }
    tokens
}

/// Count exact token matches of `word` (already lowercased) in a token list.
pub(crate) fn count_in_tokens(tokens: &[String], word: &str) -> usize {
    tokens.iter().filter(|t| t.as_str() == word).count()
}

/// Whether `word` (already lowercased) appears in a token list.
pub(crate) fn has_in_tokens(tokens: &[String], word: &str) -> bool {
    tokens.iter().any(|t| t == word)
}

#[cfg(test)]
mod tests {
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
}
