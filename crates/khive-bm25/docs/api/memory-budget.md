# Memory budget accounting

The memory API provides conservative estimates for admission control; it is not an allocator-level
measurement and intentionally excludes disposable caches.

## `memory_usage`

`Bm25Index::memory_usage` estimates owned heap storage for term keys, the structure-of-arrays
postings (`u32` document ID plus `u8` term frequency), block-max sidecars, ID maps, forward-index
strings, the `u32`-keyed document-length map, and both the `usize` and `f32` document-length
vector mirrors. It also includes approximate hash-bucket, vector, lock, configuration, and
tokenizer overhead.

The estimate uses lengths rather than vector capacities and assumes conventional 64-bit Rust
layouts: hash entries and `Arc<str>` control blocks are approximate. The IDF cache is excluded
because it is derived and can be discarded.

## `estimate_document_cost`

This method tokenizes the candidate and estimates its incremental postings, new term keys, possible
new block-max blocks, forward-index entry, document-length entry, and identity-map storage. The
external document ID component uses an average 36-byte UUID-like length because the method receives
only text.

## Budget changes

`memory_budget` returns the current optional byte limit and `set_memory_budget` replaces or clears
it. Lowering a limit below current usage does not evict documents; it affects later admissions.
Only new IDs are checked, allowing an existing document to be replaced at the limit.
