//! Tokenizer implementations: whitespace, CJK-character, keyword, identifier, unicode-word.

use crate::identifier::{is_identifier, split_identifier};
use crate::lang::is_cjk_char;
use crate::Tokenizer;

/// Splits on Unicode whitespace, trims leading/trailing ASCII punctuation from
/// each token, and drops empty results.
///
/// See `crates/khive-text/docs/api/tokenizers.md`.
#[derive(Debug, Default, Clone)]
pub struct WhitespaceTokenizer;

impl Tokenizer for WhitespaceTokenizer {
    fn tokenize(&self, text: &str) -> Vec<String> {
        text.split_whitespace()
            .map(|w| {
                w.trim_matches(|c: char| c.is_ascii_punctuation())
                    .to_string()
            })
            .filter(|t| !t.is_empty())
            .collect()
    }
}

/// Emits each CJK character as its own token; non-CJK runs split on whitespace with ASCII punctuation stripped.
///
/// See `crates/khive-text/docs/api/tokenizers.md`.
#[derive(Debug, Default, Clone)]
pub struct CjkCharTokenizer;

impl Tokenizer for CjkCharTokenizer {
    fn tokenize(&self, text: &str) -> Vec<String> {
        let mut tokens = Vec::new();
        let mut non_cjk_start = 0;

        let flush = |span: &str, out: &mut Vec<String>| {
            for part in span.split_whitespace() {
                let p = part.trim_matches(|c: char| c.is_ascii_punctuation());
                if !p.is_empty() {
                    out.push(p.to_string());
                }
            }
        };

        for (index, ch) in text.char_indices() {
            if is_cjk_char(ch) {
                flush(&text[non_cjk_start..index], &mut tokens);
                tokens.push(ch.to_string());
                non_cjk_start = index + ch.len_utf8();
            }
        }
        flush(&text[non_cjk_start..], &mut tokens);
        tokens
    }
}

/// Returns the entire (whitespace-trimmed) input as a single token.
/// Empty input → empty vec.
#[derive(Debug, Default, Clone)]
pub struct KeywordTokenizer;

impl Tokenizer for KeywordTokenizer {
    fn tokenize(&self, text: &str) -> Vec<String> {
        let t = text.trim();
        if t.is_empty() {
            vec![]
        } else {
            vec![t.to_string()]
        }
    }
}

/// Identifier-aware tokenizer: emits lowercased original + split parts for identifiers,
/// falls back to `WhitespaceTokenizer` for plain words.
///
/// See `crates/khive-text/docs/api/tokenizers.md`.
#[derive(Debug, Clone)]
pub struct IdentifierTokenizer {
    pub min_part_len: usize,
}

impl Default for IdentifierTokenizer {
    fn default() -> Self {
        Self { min_part_len: 1 }
    }
}

impl Tokenizer for IdentifierTokenizer {
    fn tokenize(&self, text: &str) -> Vec<String> {
        let ws = WhitespaceTokenizer;
        let mut result: Vec<String> = Vec::new();

        for raw_word in text.split_whitespace() {
            let word = raw_word.trim_matches(|c: char| c.is_ascii_punctuation());
            if word.is_empty() {
                continue;
            }
            if is_identifier(word) {
                let lower = word.to_lowercase();
                let parts = split_identifier(word, self.min_part_len);
                result.push(lower.clone());
                for part in parts {
                    if part != lower {
                        result.push(part);
                    }
                }
            } else {
                result.extend(ws.tokenize(word));
            }
        }
        result
    }
}

/// Splits on Unicode word boundaries using `unicode_segmentation`.
///
/// Enable with `features = ["unicode"]`.
#[cfg(feature = "unicode")]
#[derive(Debug, Default, Clone)]
pub struct UnicodeWordTokenizer;

#[cfg(feature = "unicode")]
impl Tokenizer for UnicodeWordTokenizer {
    fn tokenize(&self, text: &str) -> Vec<String> {
        use unicode_segmentation::UnicodeSegmentation;
        UnicodeSegmentation::unicode_words(text)
            .map(|w| w.to_string())
            .collect()
    }
}

#[cfg(test)]
#[path = "tokenizer/tests.rs"]
mod tests;
