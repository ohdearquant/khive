# Error taxonomy — `StorageError`

`StorageError` (`src/error.rs`) is the single error type returned across every
storage capability trait. This document covers the classifier predicates whose
full rationale does not fit inline in the rustdoc contract.

## `WriterTaskNoRuntime`

Returned instead of panicking: a caller that constructs a store from a plain,
non-async context with `KHIVE_WRITE_QUEUE=1` set gets a clean, typed failure
at first write rather than a `tokio::spawn`-outside-runtime panic. Flag-off
callers never see this variant — `writer_task_handle` only attempts to spawn
when `PoolConfig::write_queue_enabled` is set.

## `WriterTaskTerminated` and `WriterTaskRequestState`

`WriterTaskTerminated { request_state }` is the public error returned when a
writer-task request cannot complete because that task instance has terminated.
The state reports what the task can prove about the individual request:

| State                   | Meaning                                                                                                                 |
| ----------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| `NotStarted`            | The request was not accepted, or it was drained from the closed queue without invoking its operation closure            |
| `TransactionRolledBack` | A transaction-wrapped operation panicked, and the writer task successfully rolled back its enclosing SQLite transaction |
| `SideEffectsUnknown`    | The operation may have started, and the task cannot prove its final transaction or side-effect state                    |

A top-level operation has no enclosing transaction, so a panic is always
`SideEffectsUnknown`. An unexpected loss of the typed reply for an accepted
request is also classified conservatively as `SideEffectsUnknown`. A request
buffered behind the terminal request, or a send attempted after the receiver
closes, is `NotStarted`.

This error has no storage capability attribution (`capability()` returns
`None`) and is not automatically retryable (`is_retryable()` returns `false`).
In particular, callers must not blindly retry `SideEffectsUnknown`: the first
attempt may have committed a side effect. Retry decisions belong to the
operation's idempotency contract.

Neither `StorageError` nor `WriterTaskRequestState` has a serialized wire
representation. Runtime and MCP error envelopes continue to flatten storage
errors through `Display`; the rendered form is
`writer task terminated (request_state=<state>)`, where `<state>` is one of
`not_started`, `transaction_rolled_back`, or `side_effects_unknown`. This adds
typed in-process information without changing the enclosing runtime/MCP error
schema. `StorageError` is a public enum without `#[non_exhaustive]`, so adding
this variant is nevertheless a Rust source-compatibility change for downstream
code that exhaustively matches every variant; those matches must add a
`WriterTaskTerminated` arm.

## `is_fts5_syntax_error`

`TextSearch::search` returns the same `Driver` variant for a malformed MATCH
expression _and_ for a genuine backend outage (pool exhaustion, connection
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
(`crates/khive-db/src/stores/text.rs`). Pool, Timeout, Transaction, and any
other `operation` value (e.g. `fts_count`, `open_fts_reader`) always return
`false`.

Callers that fail-open the FTS leg of a hybrid search (degrading to
vector-only results on a bad query string) MUST gate on this predicate rather
than on `StorageError` broadly.

## `is_unique_constraint_violation`

`khive-db`'s `sql_bridge` labels a single-statement execute operation
differently depending on which `SqlAccess` seam produced the writer — a bare
transaction's `execute` vs. a pooled `writer()`'s `pool_writer.execute` vs.
an explicit `tx.execute` — so all three are accepted by this predicate.
Batch/script variants are intentionally excluded since a UNIQUE violation
partway through a multi-statement batch is not the same single-row-duplicate
case this predicate exists to tolerate. `pool_writer.execute` is the exact
seam `brain.record_serve` writes through.

Callers that treat exact-key duplicates as a tolerated no-op (ADR-081 §4
serve-ledger idempotency) MUST gate on this predicate rather than swallowing
every `Driver` error at `execute` — that would also hide genuine write
failures (disk full, corruption).
