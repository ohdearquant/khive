# khive-storage — core types guide

Long-form rationale extracted from doc-comments in `error.rs`, `blob.rs`, `note.rs`,
and `types/graph.rs`. Each section links back to the source item; the source
doc-comment keeps a complete standalone contract and a one-line pointer here.

## `StorageError::WriterTaskNoRuntime` (error.rs)

Returned instead of panicking: a caller that constructs a store from a plain,
non-async context with `KHIVE_WRITE_QUEUE=1` set gets a clean, typed failure
at first write rather than a `tokio::spawn`-outside-runtime panic. Flag-off
callers never see this variant — `writer_task_handle` only attempts to spawn
when `PoolConfig::write_queue_enabled` is set.

## `StorageError::is_fts5_syntax_error` (error.rs)

`TextSearch::search` returns the same `Driver` variant for a malformed MATCH
expression *and* for a genuine backend outage (pool exhaustion, connection
failure, reader open failure) — treating every `Err` as degradable turns a
real outage into a silently-empty "successful" search (issue #389). This
predicate exists to distinguish the two cases.

SQLite's FTS5 query parser (`sqlite3Fts5ParseError`, fts5_expr.c) prefixes
every message it emits with the literal `"fts5: "` token — e.g.
`fts5: syntax error near "@"`, `fts5: parser stack overflow`,
`fts5: column queries are not supported (detail=none)`. This is a stable
SQLite-internal convention, not a substring picked to match one observed
message. It excludes non-parser FTS5 subsystem failures such as
`fts5: error creating shadow table ...` (schema/storage corruption) by
requiring the message to name one of the parser's own failure modes, not
just the `fts5:` namespace prefix.

Only applies to `Driver` errors from the `Text` capability at the
`fts_search` operation — the exact seam `Fts5TextSearch::search` uses
(`crates/khive-db/src/stores/text.rs`).

## `StorageError::is_unique_constraint_violation` (error.rs)

`khive-db`'s `sql_bridge` labels a single-statement execute operation
differently depending on which `SqlAccess` seam produced the writer — a bare
transaction's `execute` vs. a pooled `writer()`'s `pool_writer.execute` vs.
an explicit `tx.execute` — so all three are accepted by this predicate.
Batch/script variants are intentionally excluded since a UNIQUE violation
partway through a multi-statement batch is not the same single-row-duplicate
case this predicate exists to tolerate. `pool_writer.execute` is the exact
seam `brain.record_serve` writes through.

## `blake3_hash_of_empty` test helper (blob.rs)

khive-storage has zero heavy dependencies (ADR-005), so this test hand-rolls
the one known `BLAKE3("")` vector instead of pulling in the `blake3` crate
just to exercise `hex_encode`.

## `deserialize_rejects_short_string` test (blob.rs)

This is the exact repro that motivates `ContentRef`'s hand-written
`Deserialize` impl: a naive derived `Deserialize` would construct
`ContentRef("x")` here, and any caller passing it to `get`/`exists`/`delete`
would then panic in `shard_path`'s `[0..2]`/`[2..4]` slices.
