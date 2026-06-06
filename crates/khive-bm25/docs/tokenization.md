# BM25 Tokenization

## Current Scope

English whitespace tokenization with lowercase normalization, stop word filtering (90 English stop
words, enabled by default), and minimum token length filtering. Covers the primary use case for
the current deployment.

Stop word filtering is controlled by `SimpleTokenizer::filter_stop_words` (default: `true`).

**Extension point**: Implement the `Tokenizer` trait in `src/tokenizer.rs` for custom tokenization.
The trait is designed to be language-agnostic and composable. The `khive-text` crate provides
additional tokenizers (CJK, identifier-aware) and filters (stemming, length) via the `Analyzer`
pipeline.

## Deferred Features (RETRIEVAL-10)

The following advanced tokenization features are intentionally deferred:

| Feature              | Status   | Rationale                            |
| -------------------- | -------- | ------------------------------------ |
| CJK segmentation     | Deferred | Requires jieba/mecab integration     |
| Arabic normalization | Deferred | Requires ICU or custom rules         |
| Stemming             | Deferred | Language-specific (Snowball, Porter) |
| Lemmatization        | Deferred | Requires NLP models                  |
| N-gram support       | Deferred | Memory/performance tradeoffs         |
