//! Composable text analysis pipeline.

use std::sync::Arc;

use crate::{Analyzer, BoxedTokenizer, TokenFilter, Tokenizer};

/// Composes one Tokenizer and N TokenFilters into a full analysis pipeline.
pub struct StandardAnalyzer {
    tokenizer: BoxedTokenizer,
    filters: Vec<Box<dyn TokenFilter>>,
}

impl StandardAnalyzer {
    /// Create an analyzer with the given tokenizer and no filters.
    pub fn with_tokenizer(tokenizer: impl Tokenizer + 'static) -> Self {
        Self {
            tokenizer: Arc::new(tokenizer),
            filters: Vec::new(),
        }
    }

    /// Append a filter to the pipeline. Filters run in insertion order.
    #[must_use]
    pub fn filter(mut self, f: impl TokenFilter + 'static) -> Self {
        self.filters.push(Box::new(f));
        self
    }
}

impl Analyzer for StandardAnalyzer {
    fn analyze(&self, text: &str) -> Vec<String> {
        let mut tokens = self.tokenizer.tokenize(text);
        for f in &self.filters {
            tokens = tokens.into_iter().filter_map(|t| f.apply(t)).collect();
        }
        tokens
    }
}

#[cfg(test)]
mod tests {
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
}
