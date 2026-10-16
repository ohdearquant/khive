//! Composable text analysis pipeline.

use std::sync::Arc;

use crate::{Analyzer, BoxedTokenizer, TokenFilter, Tokenizer};

/// Runs one tokenizer followed by filters in insertion order.
///
/// See `crates/khive-text/docs/api/analyzer-pipeline.md`.
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
#[path = "analyzer/tests.rs"]
mod tests;
