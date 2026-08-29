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
single-writer request cannot complete because its writer-task instance has terminated or the
legacy pool-mutex writer has been retired after a terminal transaction fault. The state reports
what the execution seam can prove about the individual request:

| State                   | Meaning                                                                                                                 |
| ----------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| `NotStarted`            | The request was not accepted, or it was drained from the closed queue without invoking its operation closure            |
| `TransactionRolledBack` | A transaction-wrapped operation panicked, and the writer task successfully rolled back its enclosing SQLite transaction |
| `SideEffectsUnknown`    | The operation may have started, and the task cannot prove its final transaction or side-effect state                    |

A top-level operation has no enclosing transaction, so a panic is always
`SideEffectsUnknown`. An unexpected loss of the typed reply for an accepted
request is also classified conservatively as `SideEffectsUnknown`. The same
classification applies when `COMMIT` or the request operation fails and the
writer cannot roll the enclosing transaction back, or when a transaction
terminator returns without restoring autocommit mode. A request buffered
behind the terminal request, or a send attempted after the receiver closes,
is `NotStarted`.

The legacy pool-mutex transaction fallback uses the same commit/rollback/panic finalizer as the
writer task. An unverified finalization reports `SideEffectsUnknown`; any terminal outcome retires
the pooled writer and installs a deny-all authorizer quarantine so neither later checkouts nor the
legacy raw-connection handle can reuse a connection with unknown transaction state. The variant and
its rendered `writer task terminated` prefix retain their historical names for wire and Rust API
compatibility.

If rollback after a non-panic operation or commit failure succeeds and restores autocommit mode,
no terminal error is introduced: the caller receives the original operation error or the existing
retryable commit pool error, and the writer remains available. A caught operation panic remains
terminal even when its transaction was provably rolled back.

This error has no storage capability attribution (`capability()` returns
`None`) and is not automatically retryable (`is_retryable()` returns `false`).
In particular, callers must not blindly retry `SideEffectsUnknown`: the first
attempt may have committed a side effect. Retry decisions belong to the
operation's idempotency contract.

Neither `WriterTaskTerminated` nor `WriterTaskRequestState` has a serialized
wire representation. Runtime and MCP error envelopes continue to flatten this
storage error through `Display`; the rendered form is
`writer task terminated (request_state=<state>)`, where `<state>` is one of
`not_started`, `transaction_rolled_back`, or `side_effects_unknown`. This adds
typed in-process information without changing the enclosing runtime/MCP error
schema. `StorageError` is a public enum without `#[non_exhaustive]`, so adding
this variant is nevertheless a Rust source-compatibility change for downstream
code that exhaustively matches every variant; those matches must add a
`WriterTaskTerminated` arm.

## Bounded blob read failures

`BlobTooLarge`, `BlobSizeMismatch`, and `BlobDigestMismatch` are typed
fail-closed outcomes of `BlobStore::get_bounded_verified`. All three report
`Some(StorageCapability::Blob)` from `capability()` and are non-retryable by
default. Their owned `ContentRef` fields preserve validated content-addressed
identity; no raw or malformed digest string enters the backend contract.

This addition is intentionally Rust source-breaking: `BlobStore` requires
`get_bounded_verified` with no default and no longer exposes an unbounded
whole-buffer `get`; `StorageError` is also a public enum without
`#[non_exhaustive]`. Downstream implementations must provide the bounded
method, remove any obsolete trait `get` implementation, and add arms for all
three new error variants in exhaustive matches.

`BlobTooLarge.observed_at_least` is a lower bound, not always a verified final
size. It may come from same-object metadata that caused an early refusal or
from the byte prefix that first crossed the caller's limit.
`BlobSizeMismatch` wins over digest validation once a bounded body reaches EOF,
and `BlobDigestMismatch` is evaluated only for a metadata-consistent complete
body. None of these variants carries bytes or changes the existing flattened
runtime/MCP error envelope.

## Attachment capability source compatibility

ADR-121/ADR-160 adds `StorageCapability::Attachments`. Because
`StorageCapability` is a public closed enum, downstream exhaustive matches must
add an `Attachments` arm. The new `AttachmentStore` trait does not change
existing store implementers; `EntityStore::upsert_entity_with_attachments` has
a conservative `Unsupported` default.

Removing `Entity::with_content_ref` and runtime
`create_entity_with_content_ref` is intentionally caller-source-breaking.
`Entity.content_ref` itself remains for wire/read compatibility, but it is a
read-only projection of attachment role `content`; ordinary entity upserts
ignore it.

## Typed writer-pool checkout timeout source

`khive-db` retains `SqliteError::WriterPoolCheckoutTimeout` as the typed source
inside `StorageError::Driver`. Runtime downcasts that source without inspecting
its message, preserving the wrapping capability and operation. MCP is the one
intentional exception to the default flat `Display` wire form: it emits stable
`code` and `stage` fields of `writer_pool_checkout_timeout`, `timeout_ms`, and
the wrapper's `capability`/`operation`. Its `message` field keeps the historical
rendering for compatibility. This timeout occurs before SQLite executes and
must not be classified as `SQLITE_BUSY` or checkpoint starvation.

## Typed writer-task BEGIN contention

`StorageError::WriterTaskBusy { timeout_ms }` means the writer queue accepted
and dequeued the request, but SQLite returned `SQLITE_BUSY` or `SQLITE_LOCKED`
until the connection's configured busy timeout expired. The writer task never
invoked the request operation, so retrying that one failed operation is safe.
The variant is capability-neutral and `is_retryable()` returns `true`.

MCP preserves this proof with `code`/`stage` set to
`writer_task_begin_busy`, `operation: "writer_task_begin"`, and
`retryable: true`. Its `scope` and `retry_after_ms` are null: unlike
`writer_queue_saturated`, the queue did accept this request, and no separate
backoff policy is defined. Other `BEGIN IMMEDIATE` failures retain the generic
pool error and are not promoted by rendered-message matching.

## Typed storage-admission timeout

`StorageError::AdmissionTimeout { operation, timeout_ms }` means a bounded
wait for storage admission — a reader/writer handle slot or a pooled reader
checkout — elapsed before anything was acquired. The operation never started,
so retrying cannot duplicate a side effect. This is distinct from
`StorageError::Timeout`, which makes no claim about whether work was in
flight when the deadline expired; only the admission variant is promoted to
a structured retryable failure, and only by its typed variant, never by
rendered-message matching.

One carve-out: the raw-SQL reader admission paths (`sql_bridge.reader_open`
and `sql_bridge.reader_operation`) keep returning `StorageError::Timeout` on
saturation, as the ADR-005 reader-admission amendment requires. The typed
admission variant covers the writer-handle, atomic-unit, and pooled-reader
checkout budgets.

MCP emits `code`/`stage` of `storage_admission_timeout` with the failing
`operation`, the elapsed `timeout_ms`, and `retryable: true`. `capability`,
`scope`, and `retry_after_ms` are null: the handle-slot and reader-checkout
budgets are capability-neutral and no separate backoff policy is defined.

## Typed cached-reader read-transaction age eviction

`sql_bridge`'s cached-reader read path proactively rolls back an admitted
read transaction once it has pinned a WAL snapshot past the configured
`read_tx_max_age` (#1846). Two typed, capability-neutral, `is_retryable() ==
true` variants report the outcome, both public and both introduced by this
change:

- `StorageError::ReadTransactionAgeEvicted { operation, max_age_secs }` — the
  `ROLLBACK` succeeded and autocommit was restored; the connection returns to
  the pool ready for a fresh read snapshot.
- `StorageError::ReadTransactionAgeEvictionCleanupFailed { operation,
  max_age_secs, message }` — the `ROLLBACK` was denied or errored, or it
  reported success without actually restoring autocommit. The connection is
  discarded instead of being returned to the pool. `message` names which of
  the two cleanup failures occurred.

Both are always safe to retry: the age check runs before any read on the
connection, so no side effect exists for a retry to duplicate, regardless of
which cleanup outcome followed. Both are distinct from the generic
`StorageError::Transaction` variant, whose other cases (write-side ambiguity,
unrelated rollback failures) are not uniformly safe to retry — callers must
not detect this condition by parsing rendered text.

MCP maps both variants to the same `code`/`stage` of `read_tx_age_evicted`
(`khive_runtime::error::READ_TX_AGE_EVICTED_STAGE`), the failing `operation`,
`capability: "sql"`, `retryable: true`, and `timeout_ms` set to
`max_age_secs * 1000`. `scope` and `retry_after_ms` are null. The rendered
`message` field is the only wire-visible way to distinguish a clean eviction
from a failed cleanup.

`StorageError` is a public enum without `#[non_exhaustive]`, so adding these
two variants is a Rust source-compatibility change for downstream code that
exhaustively matches every variant; those matches must add
`ReadTransactionAgeEvicted` and `ReadTransactionAgeEvictionCleanupFailed`
arms.

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
