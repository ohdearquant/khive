//! Named analyzer constructors for common use cases.

use crate::analyzer::StandardAnalyzer;
use crate::filter::{LowercaseFilter, MaxLengthFilter, MinLengthFilter, StopWordFilter};
use crate::tokenizer::{CjkCharTokenizer, KeywordTokenizer, WhitespaceTokenizer};

/// Standard English analyzer: whitespace + lowercase + stop words + length filters.
///
/// Produces the same token stream as `khive-bm25`'s `SimpleTokenizer::default()`,
/// but returns a `StandardAnalyzer` rather than a `khive-bm25` tokenizer type.
pub fn standard() -> StandardAnalyzer {
    StandardAnalyzer::with_tokenizer(WhitespaceTokenizer)
        .filter(LowercaseFilter)
        .filter(StopWordFilter)
        .filter(MinLengthFilter(2))
        .filter(MaxLengthFilter(40))
}

/// Simple analyzer: whitespace split + lowercase only.
pub fn simple() -> StandardAnalyzer {
    StandardAnalyzer::with_tokenizer(WhitespaceTokenizer).filter(LowercaseFilter)
}

/// Keyword analyzer: entire input is one lowercased token.
pub fn keyword() -> StandardAnalyzer {
    StandardAnalyzer::with_tokenizer(KeywordTokenizer)
        .filter(LowercaseFilter)
        .filter(MaxLengthFilter(200))
}

/// CJK analyzer: character-level unigrams for CJK, whitespace for Latin.
pub fn cjk() -> StandardAnalyzer {
    StandardAnalyzer::with_tokenizer(CjkCharTokenizer)
        .filter(LowercaseFilter)
        .filter(MinLengthFilter(1))
        .filter(MaxLengthFilter(40))
}

/// KG entity name analyzer: identifier-aware splitting + lowercase.
pub fn kg_name() -> StandardAnalyzer {
    use crate::tokenizer::IdentifierTokenizer;
    StandardAnalyzer::with_tokenizer(IdentifierTokenizer::default())
        .filter(LowercaseFilter)
        .filter(MinLengthFilter(1))
        .filter(MaxLengthFilter(80))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Analyzer;

    #[test]
    fn standard_english() {
        let a = standard();
        let tokens = a.analyze("The quick brown fox jumps over the lazy dog");
        assert_eq!(
            tokens,
            vec!["quick", "brown", "fox", "jumps", "lazy", "dog"]
        );
    }

    #[test]
    fn standard_drops_short_and_stops() {
        let a = standard();
        let tokens = a.analyze("I am a test");
        assert_eq!(tokens, vec!["test"]);
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
    fn standard_drops_long_tokens() {
        let long = "a".repeat(50);
        let input = format!("hello {long} world");
        let tokens = standard().analyze(&input);
        assert_eq!(tokens, vec!["hello", "world"]);
    }
}
