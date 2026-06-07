//! Tokenizer implementations: whitespace, CJK-character, keyword, identifier, unicode-word.

use crate::identifier::{is_identifier, split_identifier};
use crate::lang::is_cjk_char;
use crate::Tokenizer;

// ---------------------------------------------------------------------------
// WhitespaceTokenizer
// ---------------------------------------------------------------------------

/// Splits on ASCII whitespace, trims leading/trailing ASCII punctuation from
/// each token, and drops empty results.
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

// ---------------------------------------------------------------------------
// CjkCharTokenizer
// ---------------------------------------------------------------------------

/// Emits each CJK character as its own token; non-CJK runs split on whitespace with ASCII punctuation stripped.
#[derive(Debug, Default, Clone)]
pub struct CjkCharTokenizer;

impl Tokenizer for CjkCharTokenizer {
    fn tokenize(&self, text: &str) -> Vec<String> {
        let mut tokens = Vec::new();
        let mut latin_buf = String::new();

        let flush = |buf: &mut String, out: &mut Vec<String>| {
            for part in buf.split_whitespace() {
                let p = part.trim_matches(|c: char| c.is_ascii_punctuation());
                if !p.is_empty() {
                    out.push(p.to_string());
                }
            }
            buf.clear();
        };

        for ch in text.chars() {
            if is_cjk_char(ch) {
                flush(&mut latin_buf, &mut tokens);
                tokens.push(ch.to_string());
            } else {
                latin_buf.push(ch);
            }
        }
        flush(&mut latin_buf, &mut tokens);
        tokens
    }
}

// ---------------------------------------------------------------------------
// KeywordTokenizer
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// IdentifierTokenizer
// ---------------------------------------------------------------------------

/// Identifier-aware tokenizer: emits lowercased original + split parts for identifiers,
/// falls back to `WhitespaceTokenizer` for plain words.
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

// ---------------------------------------------------------------------------
// UnicodeWordTokenizer (feature = "unicode")
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- WhitespaceTokenizer ---

    #[test]
    fn whitespace_normal() {
        let t = WhitespaceTokenizer;
        assert_eq!(t.tokenize("hello world"), vec!["hello", "world"]);
    }

    #[test]
    fn whitespace_empty() {
        assert!(WhitespaceTokenizer.tokenize("").is_empty());
        assert!(WhitespaceTokenizer.tokenize("   ").is_empty());
    }

    #[test]
    fn whitespace_strips_outer_punct() {
        let t = WhitespaceTokenizer;
        assert_eq!(t.tokenize("hello, world!"), vec!["hello", "world"]);
        assert_eq!(t.tokenize("...foo..."), vec!["foo"]);
    }

    #[test]
    fn whitespace_preserves_inner_punct() {
        let t = WhitespaceTokenizer;
        assert_eq!(t.tokenize("fine-tuning v1.2"), vec!["fine-tuning", "v1.2"]);
    }

    #[test]
    fn whitespace_all_punct_dropped() {
        assert!(WhitespaceTokenizer.tokenize("... , ...").is_empty());
    }

    // --- CjkCharTokenizer ---

    #[test]
    fn cjk_spec_example() {
        let got = CjkCharTokenizer.tokenize("使用LoRA进行fine-tuning");
        assert_eq!(got, vec!["使", "用", "LoRA", "进", "行", "fine-tuning"]);
    }

    #[test]
    fn cjk_empty() {
        assert!(CjkCharTokenizer.tokenize("").is_empty());
    }

    #[test]
    fn cjk_only_cjk() {
        assert_eq!(CjkCharTokenizer.tokenize("中文"), vec!["中", "文"]);
    }

    #[test]
    fn cjk_only_latin() {
        assert_eq!(
            CjkCharTokenizer.tokenize("hello world"),
            vec!["hello", "world"]
        );
    }

    #[test]
    fn cjk_punct_stripped_from_latin_run() {
        let got = CjkCharTokenizer.tokenize("你好, world!");
        assert_eq!(got, vec!["你", "好", "world"]);
    }

    #[test]
    fn cjk_hangul_unigrams() {
        assert_eq!(CjkCharTokenizer.tokenize("가나"), vec!["가", "나"]);
    }

    // --- KeywordTokenizer ---

    #[test]
    fn keyword_whole_phrase() {
        assert_eq!(
            KeywordTokenizer.tokenize("attention is all you need"),
            vec!["attention is all you need"]
        );
    }

    #[test]
    fn keyword_empty() {
        assert!(KeywordTokenizer.tokenize("").is_empty());
        assert!(KeywordTokenizer.tokenize("   ").is_empty());
    }

    #[test]
    fn keyword_trims_outer_whitespace() {
        assert_eq!(KeywordTokenizer.tokenize("  hello  "), vec!["hello"]);
    }

    #[test]
    fn keyword_single_word() {
        assert_eq!(KeywordTokenizer.tokenize("LoRA"), vec!["LoRA"]);
    }

    // --- IdentifierTokenizer ---

    #[test]
    fn identifier_spec_example() {
        let t = IdentifierTokenizer::default();
        // "LoRA" is_identifier → lora + lo + ra; "attention" is plain → pass-through
        let got = t.tokenize("LoRA attention");
        assert_eq!(got, vec!["lora", "lo", "ra", "attention"]);
    }

    #[test]
    fn identifier_empty() {
        assert!(IdentifierTokenizer::default().tokenize("").is_empty());
    }

    #[test]
    fn identifier_plain_passthrough() {
        assert_eq!(
            IdentifierTokenizer::default().tokenize("attention"),
            vec!["attention"]
        );
    }

    #[test]
    fn identifier_snake_case() {
        let got = IdentifierTokenizer::default().tokenize("fine_tuning");
        assert_eq!(got, vec!["fine_tuning", "fine", "tuning"]);
    }

    #[test]
    fn identifier_kebab() {
        let got = IdentifierTokenizer::default().tokenize("fine-tuning");
        assert_eq!(got, vec!["fine-tuning", "fine", "tuning"]);
    }

    #[test]
    fn identifier_min_part_len_filters_short() {
        let t = IdentifierTokenizer { min_part_len: 3 };
        // "LoRA" parts: "lo" (2), "ra" (2) — both below min=3
        assert_eq!(t.tokenize("LoRA"), vec!["lora"]);
    }

    #[test]
    fn identifier_punct_stripped() {
        let got = IdentifierTokenizer::default().tokenize("LoRA,");
        assert_eq!(got, vec!["lora", "lo", "ra"]);
    }

    // --- UnicodeWordTokenizer ---

    #[cfg(feature = "unicode")]
    #[test]
    fn unicode_word_basic() {
        let got = UnicodeWordTokenizer.tokenize("Hello, world!");
        assert_eq!(got, vec!["Hello", "world"]);
    }

    #[cfg(feature = "unicode")]
    #[test]
    fn unicode_word_empty() {
        assert!(UnicodeWordTokenizer.tokenize("").is_empty());
    }
}
