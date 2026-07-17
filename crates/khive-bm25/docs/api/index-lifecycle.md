# Index lifecycle and persisted state

`Bm25Index` owns document identity, postings, corpus statistics, and the derived caches needed by
search. This reference describes construction, mutation, serialization, and recovery behavior.

## Configuration and construction

`Bm25Config::try_new(k1, b)` rejects non-finite or negative `k1` and any `b` outside `[0, 1]`.
`Bm25Config::new` stores the values unchecked — it neither validates nor panics. The deprecated
`Bm25Index::new` validates and panics on invalid inputs; callers handling untrusted
configuration should use `try_new`. Defaults are `k1 = 1.2` and
`b = 0.75`.

`Bm25Index::try_with_tokenizer` applies the same validation while installing a caller-supplied
`Arc<dyn Tokenizer>`. Changing the tokenizer later affects only future indexing: existing postings
are not re-tokenized.

## `index_document`

Indexing tokenizes and builds the replacement term-frequency map before mutating the index. Empty
replacement text is ignored, so re-indexing an existing document with no indexable terms preserves
the old document. A new document is rejected with `RetrievalError::BudgetExceeded` when its
estimated cost would cross the configured budget; re-indexing an existing ID bypasses that check.

Posting lists remain sorted by internal document ID for binary-search seeks. Term frequency is
stored as `u8` and clamps at 255; at the default `k1`, BM25 saturation makes the resulting early
plateau negligible for ordinary documents. `total_tokens` uses saturating addition, so extreme
corpora lose average-length precision rather than overflow.

## `remove_document`

Removal returns `false` when the external ID is absent. The forward index makes a successful
removal proportional to the terms in that document rather than the full vocabulary. Internal
numeric IDs are never reused; the reverse vector retains a hole after removal.

Every add or remove increments the postings epoch. IDF detects a changed document count on the next
search, while block-max metadata rebuilds lazily when its epoch is stale.

## Document identifiers

`DocumentId` is a transparent string newtype: its JSON representation is a bare string, not an
object. The wire fixture deliberately locks this representation; changing it requires an explicit
migration plan.

## Deserialization validation

The custom `Deserialize` implementation validates configuration, a non-zero block size, the
bidirectional external/internal ID maps, posting references, and the checked sum of document
lengths against `total_tokens`. It then rebuilds the SIMD document-length vectors, forward index,
IDF cache state, and block-max state. Invalid snapshots fail before a usable index is returned.
