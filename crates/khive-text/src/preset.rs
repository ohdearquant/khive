//! Named analyzer constructors for common use cases.

use crate::analyzer::StandardAnalyzer;
use crate::filter::{Bm25StopWordFilter, LowercaseFilter, MaxLengthFilter, MinLengthFilter};
use crate::tokenizer::{CjkCharTokenizer, KeywordTokenizer, WhitespaceTokenizer};

/// Standard English analyzer: whitespace + lowercase + stop words.
///
/// Produces the same token stream as `khive-bm25`'s `SimpleTokenizer::default()`,
/// but returns a `StandardAnalyzer` rather than a `khive-bm25` tokenizer type.
pub fn standard() -> StandardAnalyzer {
    StandardAnalyzer::with_tokenizer(WhitespaceTokenizer)
        .filter(LowercaseFilter)
        .filter(Bm25StopWordFilter)
        .filter(MinLengthFilter(1))
}

/// Simple analyzer: whitespace split + lowercase only.
pub fn simple() -> StandardAnalyzer {
    StandardAnalyzer::with_tokenizer(WhitespaceTokenizer).filter(LowercaseFilter)
}

/// Keyword analyzer: entire input is one lowercased token, regardless of length.
pub fn keyword() -> StandardAnalyzer {
    StandardAnalyzer::with_tokenizer(KeywordTokenizer).filter(LowercaseFilter)
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

    #[test]
    fn standard_matches_bm25_default_token_stream() {
        let bm25 = khive_bm25::SimpleTokenizer::default();
        let cases = [
            "may x",
            "done say might",
            "against",
            &"a".repeat(41),
            "The quick brown fox jumps over the lazy dog",
        ];
        for input in cases {
            let ours = standard().analyze(input);
            let theirs = khive_bm25::Tokenizer::tokenize(&bm25, input);
            assert_eq!(ours, theirs, "mismatch for input: {input:?}");
        }
    }
}
