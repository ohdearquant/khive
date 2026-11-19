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
#[path = "matching_tests.rs"]
mod tests;
