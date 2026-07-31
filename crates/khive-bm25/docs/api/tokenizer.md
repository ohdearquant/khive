# Tokenizer API

Tokenizers transform input text into the terms stored in the BM25 inverted index.

## `Tokenizer`

Implementations are `Send + Sync` and return an empty vector for empty input. `BoxedTokenizer` is
an `Arc<dyn Tokenizer>`, allowing an index and its clones to share tokenizer state safely.

## `SimpleTokenizer`

`SimpleTokenizer` splits on whitespace, strips leading and trailing ASCII punctuation (non-ASCII
punctuation is retained), optionally lowercases, filters by minimum Unicode character count, and
optionally removes the built-in English stop-word set. Defaults enable lowercasing and stop-word
removal with a minimum length of one.

Punctuation trimming always drops an empty result before the length filter runs. A `min_length` of
zero is therefore supported as “no minimum for non-empty normalized terms”; it never causes a
punctuation-only span such as `!!!` to emit an empty-string term.

The tokenizer composes `khive-text`'s shared whitespace, lowercase, length, and BM25 stop-word
primitives. `Tokenizer` and `BoxedTokenizer` are re-exported from `khive-text`, so custom tokenizer
implementations keep the same public interface.

Indexes created before the `khive-text` tokenizer adoption with `SimpleTokenizer::new(_, 0)` may
contain empty-string postings from punctuation-only spans. Deserialization does not re-tokenize
stored documents. Rebuild those indexes from source text after upgrading; see
[Index lifecycle and persisted state](index-lifecycle.md#tokenizer-changes-and-rebuilds).

## `tokenize`

The free function constructs `SimpleTokenizer::default()` for one call. Repeated indexing should
prefer a persistent `Bm25Index` tokenizer rather than repeatedly constructing configuration.
