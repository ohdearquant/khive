# BM25 Tokenization

## Current Scope

English whitespace tokenization with optional lowercase normalization and minimum token length
filtering. Covers the primary use case for the current deployment.

**Extension point**: Implement the `Tokenizer` trait in `src/tokenizer.rs` for custom tokenization.
The trait is designed to be language-agnostic and composable.

## Deferred Features (RETRIEVAL-10)

The following advanced tokenization features are intentionally deferred:

| Feature              | Status   | Rationale                            |
| -------------------- | -------- | ------------------------------------ |
| CJK segmentation     | Deferred | Requires jieba/mecab integration     |
| Arabic normalization | Deferred | Requires ICU or custom rules         |
| Stemming             | Deferred | Language-specific (Snowball, Porter) |
| Lemmatization        | Deferred | Requires NLP models                  |
| Stop word removal    | Deferred | Language and domain specific         |
| N-gram support       | Deferred | Memory/performance tradeoffs         |
