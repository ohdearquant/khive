use super::*;
use crate::filter::{LowercaseFilter, MinLengthFilter};
use crate::tokenizer::WhitespaceTokenizer;

#[test]
fn pipeline_lowercases() {
    let a = StandardAnalyzer::with_tokenizer(WhitespaceTokenizer).filter(LowercaseFilter);
    assert_eq!(a.analyze("Hello WORLD"), vec!["hello", "world"]);
}

#[test]
fn pipeline_chains_filters() {
    let a = StandardAnalyzer::with_tokenizer(WhitespaceTokenizer)
        .filter(LowercaseFilter)
        .filter(MinLengthFilter(4));
    assert_eq!(a.analyze("hi hello world"), vec!["hello", "world"]);
}

#[test]
fn empty_input() {
    let a = StandardAnalyzer::with_tokenizer(WhitespaceTokenizer);
    assert!(a.analyze("").is_empty());
}
