# Tokenizer API

Tokenizers transform input text into the terms stored in the BM25 inverted index.

## `Tokenizer`

Implementations are `Send + Sync` and return an empty vector for empty input. `BoxedTokenizer` is
an `Arc<dyn Tokenizer>`, allowing an index and its clones to share tokenizer state safely.

## `SimpleTokenizer`

`SimpleTokenizer` splits on whitespace, strips leading and trailing ASCII punctuation (non-ASCII
punctuation is retained), optionally lowercases, filters by minimum UTF-8 byte length, and
optionally removes the built-in English stop-word set. Defaults enable lowercasing and stop-word
removal with a minimum length of one.

ASCII tokens use an ASCII-only lowercase fast path; non-ASCII input falls back to Unicode-aware
lowercasing. Capacity is estimated from typical English word length to reduce reallocations without
affecting output.

## `tokenize`

The free function constructs `SimpleTokenizer::default()` for one call. Repeated indexing should
prefer a persistent `Bm25Index` tokenizer rather than repeatedly constructing configuration.
