//! Named analyzer constructors for common use cases.

use crate::analyzer::StandardAnalyzer;
use crate::filter::{Bm25StopWordFilter, LowercaseFilter, MaxLengthFilter, MinLengthFilter};
use crate::tokenizer::{CjkCharTokenizer, KeywordTokenizer, WhitespaceTokenizer};

/// Return the whitespace/lowercase/stop-word pipeline matching BM25's default tokenizer.
///
/// See `crates/khive-text/docs/api/analyzer-pipeline.md`.
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
#[path = "preset/tests.rs"]
mod tests;
