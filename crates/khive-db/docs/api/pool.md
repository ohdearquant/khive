# Connection Pool

`ConnectionPool` (`crates/khive-db/src/pool.rs`) owns the SQLite connection(s)
for a single database file and, when the write queue is enabled, the one
`WriterTask` that serializes every mutating statement through it. This is the
function-specific technical reference for the pool's private/internal
mechanics and the tests that pin them down; see `crates/khive-db/docs/design.md`
("Single-Writer Write Queue") for the ADR-067 rationale.

Runtime write-route classification and checkpoint-ownership coordination live in
`crates/khive-db/src/pool/admission.rs`. The public
`khive_db::pool::RuntimeWriteOperation` path and the `ConnectionPool` entrypoints
remain in `pool`; disk-space admission and write units live in
`crates/khive-db/src/pool/write_units.rs`.

## Physical database identity

On the first writable open of a new or legacy file-backed database, the pool
creates `main._khive_database_identity`, a singleton table containing a UUID.
This is a persistent startup write and a schema change. The pool reads the
UUID from each subsequently opened connection to detect a swapped main file.
It is pool infrastructure rather than a service table: the pool needs the
identity before `apply_schema()`, and it also opens secondary databases with
independent migration ledgers. For that reason, this table is installed by the
pool instead of a numbered service migration. Read-only pools never install it.

At or below the write reserve, a writable pool skips installation when the
table is absent, so startup and checkpoint infrastructure remain available.
Write admissions still fail at the reserve. A pool with no UUID pinned accepts
a later installation by another process; on Unix, device/inode checks still
guard new connections to its opened file. The next writable pool open above
the reserve installs and pins the UUID. The startup transaction uses the
configured busy timeout.

## SQLite write reserve

Writable file-backed pools take their free-space reserve from the resolved
disk-guard policy. A backend's own reserve setting wins, then
`KHIVE_SQLITE_DISK_RESERVE_BYTES`, then the deprecated
`KHIVE_DB_FREE_SPACE_FLOOR_BYTES` (still read, with a warning), then the 1 GiB
default. Setting both variables to different values is a configuration error,
even when a backend overrides them. The policy is resolved once when the pool
opens, and invalid numeric settings fail construction with
`SqliteError::InvalidConfig`. A reserve of `0` removes the floor comparison,
but still requires volume identity, the cooperative lease and a successful
probe; operation-specific headroom, including VACUUM's estimate, still applies.
In-memory and read-only pools do not take the disk lease or sample disk space.

`disk_guard_deadline_ms` resolves from the backend setting, then
`KHIVE_SQLITE_DISK_GUARD_DEADLINE_MS`, then 2,000 ms. Its valid range is
100–10,000 ms without clamping. It bounds volume-lease acquisition independently
of the pool's `checkout_timeout` and the queue-only
`write_admission_deadline_ms`.

Writable file-backed hosts also need an absolute volume-lock directory shared
by cooperating khive processes of the user. The default resolver uses a
non-blank `KHIVE_VOLUME_LOCK_DIR`, otherwise `.khive/sqlite-volume-locks` under
the first non-blank of `HOME` and `USERPROFILE`. There is no working-directory
fallback; an unresolved or relative default is `SqliteError::InvalidConfig`.
`ConnectionPool::new` requires a supplied, absolute directory before any SQLite
open: a missing directory is a configuration error, and an explicitly supplied
relative directory is a `CapacityUnavailable` lock-phase error. Read-only and
in-memory pools do not require it. Test fixtures may supply their own directory;
the test-harness default uses a separate temporary lock namespace.

Admission identifies the volume through the nearest existing canonical ancestor
of the database path, resolved once when the pool is built. On Unix, a database
file that exists at that point is itself the probe target, not its parent; a
missing file resolves through an existing ancestor, and a pool that creates its
database keeps probing that ancestor.
On Windows the capacity probe uses the resolved volume root. The stable volume
key selects the cooperative lease, while the path is diagnostic/probe metadata.

An admitted pooled checkout takes the volume lease before the writer mutex and
leaves capacity sampling to the logical-write boundary: a transaction samples
after successful `BEGIN IMMEDIATE`, before its body; an autocommit unit samples
immediately before its write. The public compatibility `writer()` path also
samples at checkout, which alone cannot authorize arbitrary later raw SQL.
Writer-task transactional requests sample after `BEGIN`, and admitted standalone
transactions follow the same order; writer-task top-level requests run without a
transaction and sample before they run. Standalone autocommit statements/scripts
sample before their first write call. Each admission takes a fresh sample; the result is not
cached across operations. The lease stays held through commit, rollback or
owned-connection settlement.

Admission compares available space with the reserve plus required headroom. If
that sum is nonzero, space at or below it returns `CapacityFloor` before the
logical write runs; addition overflow also refuses. `CapacityUnavailable` distinguishes
`identity`, `lock` and `probe` failures; neither capacity outcome requests an
automatic retry. A thread that already holds the same volume lease is refused
immediately with `VolumeLeaseReentry`, naming the holder and requester sites.
Cross-thread contention instead waits within the guard deadline. A manual atomic
unit's lease is detached from the thread that took it, so a later request from
that thread waits like any other contender rather than reading as re-entry.

A capacity refusal whose rollback restores autocommit leaves the writer reusable.
A pooled `WriterGuard` rollback that does not prove autocommit retires that writer
and returns `WriterSettlementUnknown`. This result is distinct from subsequent
owned cleanup, which runs while the lease is held and may still restore autocommit
or close the connection successfully. Only if that cleanup proves neither
autocommit nor successful close is shared admission poisoned, so later writes
fail with `WriterPoisoned` before starting. Storage
projection distinguishes the unknown earlier outcome (`SideEffectsUnknown`) from
a later refused write (`NotStarted`). The writer task additionally retires its
queue on a terminal request outcome.

Checkpoint and read-only diagnostics bypass disk-lease and capacity admission,
including `try_checkpoint_nowait` and caller-issued
`TopLevelMaintenance::WalCheckpointTruncate` on both writer-task and standalone
SQL routes. The queued route uses `send_checkpoint_bounded`; the standalone
route takes a lease only for `Vacuum`. VACUUM is not a recovery bypass: it needs
the lease and a copy-sized estimate from database/WAL metadata, and an unavailable
estimate, arithmetic overflow or insufficient headroom refuses it. A checkpoint
bypass does not suppress SQLite errors or bypass the route's other availability
checks. Pool startup remains available below the floor: startup connection
setup can perform I/O, and bootstrap DDL has its own admitted boundary.

An already-admitted large transaction or another filesystem consumer can still
exhaust space. The sampled floor is neither reserved capacity nor guaranteed
recovery headroom, and it is not a transaction-size quota. See
[SQLite disk admission](disk-admission.md) for the boundary inventory and remaining
raw-connection work. Full ADR-154 conformance remains tracked by
[#3551](https://github.com/ohdearquant/khive/issues/3551); native `SQLITE_FULL`
code propagation after automatic rollback remains separate work in
[#4405](https://github.com/ohdearquant/khive/issues/4405).

`try_checkpoint_nowait` returns a restricted `CheckpointGuard`. It has fixed
PASSIVE and TRUNCATE operations and exposes no raw SQLite connection or
general SQL execution. A busy pooled writer causes an immediate skip; the
scheduled ADR-091 task continues to use its dedicated connection.

## WAL autocheckpoint ownership

Routine WAL reclamation has exactly two owners, selected by whether a dedicated
checkpoint task actually runs against the pool:

- **Claimed** — the scheduled ADR-091 checkpoint task calls
  `ConnectionPool::claim_checkpoint_ownership` at startup (and
  `propagate_checkpoint_claim_to_writer_task` for a writer task spawned before
  the claim). From then on every writer-capable connection sets
  `PRAGMA wal_autocheckpoint = 0`: the pool's startup writer is re-configured
  under the writer mutex, and every later open through the standalone boundary
  (store writers, raw SQL bridge writers, the writer task, diagnostics, the
  dedicated checkpoint connection) inherits the setting. Routine checkpoint
  I/O stays off application commit paths; the task's separately bounded
  TRUNCATE policy is unchanged.
- **Unclaimed** — no checkpoint task runs (embedded runtimes, one-shot CLI
  executions). Writer-capable connections keep a bounded autocheckpoint
  (4,000 pages), so SQLite's own reclamation prevents unbounded WAL growth
  and eventual disk exhaustion on writable pools that have no other
  checkpoint owner.

There is no environment or `PoolConfig` override in either direction: the only
way to move between the two postures is an actual ownership claim, which only
the scheduled checkpoint task makes. The standalone embedding-model registry
query is opened read-only and is not a third writer constructor.

## `ConnectionPool::writer_task_handle` — single-writer-task rationale

See `crates/khive-db/src/pool.rs` — `writer_task_handle`.

Exactly one writer task exists per `ConnectionPool` (per DB file) no matter
how many stores or namespaces are constructed over it: the `OnceLock` runs
its init closure at most once, so concurrent callers either race to run it
once or block on the in-flight init and then all receive a clone of the same
resulting handle. This is what makes the write queue an actual
single-writer core rather than one writer task per store — a per-store
writer task would let concurrent migrated stores over the same pool spawn
independent writer connections that contend with each other at `BEGIN
IMMEDIATE`, defeating the point of ADR-067 Component A.

## `ConnectionPool::writer_task_for_write` — write-time store routing (#1847)

Store construction is synchronous and can happen before a Tokio runtime is
entered. A constructor may therefore cache no writer-task handle even though a
later write runs inside a runtime where the pool can spawn (or retrieve) its
single task. Every SQLite-backed store resolves through
`writer_task_for_write` at the write seam, including transaction-owning and
batch entry points, so a construction-time `None` is never a permanent routing
decision.

When `write_routing_strict` is enabled, a missing handle fails closed: a
missing runtime preserves `StorageError::WriterTaskNoRuntime`, while an
explicitly disabled or degraded queue returns a typed `StorageError::Pool`
naming the operation. In compatibility mode, a caller may use its legacy
standalone/pool-mutex writer only after this lookup returns `None`; that exact
fallback emits a store-specific `direct_route_violation` when the file-backed
queue is enabled.

This routing hardening does **not** flip the strict-mode default. ADR-135 F2
and ADR-136 D2 still require production A/B evidence and a release gate before
`write_routing_strict` can become the default; until that evidence is accepted,
`KHIVE_WRITE_ROUTING=strict` remains opt-in.

## `ConnectionPool::writer_task_for_runtime_write`

Entity merge, note merge, and symmetric edge update use this adapter for their
runtime-owned SQL transactions. The closed `RuntimeWriteOperation` enum supplies
the operation name and private telemetry classification without exposing the
sink. The adapter applies `writer_task_for_write`'s strict refusal before the
caller can enter its direct transaction. A returned `None` permits only the
existing compatibility fallback and records its operation-specific violation
when the file-backed queue is enabled. Explicit queue opt-out and in-memory
pools remain excluded from violation telemetry.

## Test coverage notes

### `writer_guard_transaction_registers_during_closure_only`

ADR-091 Plank 0: `WriterGuard::transaction` registers an entry with the
shared open-transaction registry for the duration of the closure, and
deregisters it once the closure (and its commit/rollback) completes.

`#[serial(tx_registry)]`: the open-transaction registry is a process-wide
singleton (`khive_storage::tx_registry`) shared across every test in this
binary. This test filters by its own unique label so it is not vulnerable to
another test's entry being reported as "oldest", but it still shares the
same `tx_registry` serial group as `checkpoint.rs`'s and `sql_bridge.rs`'s
registry tests for defense-in-depth against cross-test interference.

## Pooled reader routing and raw-SQL exception budget

Every typed store read waits for its permit on the async side with
`ConnectionPool::acquire_reader_admission`, so a read that has to queue holds a
task and not a blocking-pool thread, then selects its connection with
`ConnectionPool::reader_with_admission`, for both file-backed and in-memory
databases. A file-backed `SqlAccess::reader()` is only a logical
handle: constructing or retaining it opens no SQLite connection. Its ordinary
`query_row`, `query_all`, and `query_page` calls check out one pooled reader for
the operation and return it only after statement finalization, callback cleanup,
and connection reset/replacement. Ordinary reads through a queue-backed
`SqlWriter` use the same route. There is no standalone fallback after pool
saturation. A raw-SQL read waits for its permit the same way, so raw-SQL and
typed reads are admitted in arrival order.

The pool-wide reader admission budget is the effective `max_readers` (minimum
one in degraded in-memory mode). Pooled guards and the explicit raw-SQL
transaction exception below share that budget, so their combined live work
cannot exceed it. A real wait that exhausts `checkout_timeout` before work
begins returns retryable `StorageError::AdmissionTimeout`; cancellation or an
expired request context returns non-retryable `StorageError::Timeout`. Raw-SQL
ordinary reads name `sql_bridge.reader_operation`; typed stores retain their
capability operation name.

Pooled-reader admission refusals include `pool_identity: Some(...)` alongside
the unchanged operation and timeout. File-backed pools report only the final
file name of their canonical path, never a directory, on every server including
local stdio. In-memory pools use `:memory:`. Display appends
`(pool: {identity})`, and the structured error includes the same optional
`pool_identity` string. File names render lossily when they are not UTF-8.

If distinct canonical paths with the same rendered file name are open in the
process at refusal time, each reports `<filename>#<8 hex>`. The suffix is a
SHA-256 digest of the canonical path: raw bytes on Unix, UTF-16 little-endian
code units on Windows (lossy UTF-8 on other platforms). The first eight lowercase
hex digits are stable across processes, restarts and builds for the same path
bytes. This unkeyed, truncated hash can be matched against guessed paths and can
collide; it does not provide secrecy or a globally unique identifier. Repeated opens of
the same canonical path share a registration and never cause a suffix on their
own. They share the same suffix if a different colliding store is also open.
The suffix disappears once no other colliding store remains open. Registration
is reference-counted and removed when the last pool for that path drops. A
failed pool constructor leaves no registration. This is a process-local store
label, not a globally unique ID, a pool-instance ID, or a pool role. It describes no
remote process's pools; all in-memory pools share the memory label.

`KHIVE_CHECKOUT_TIMEOUT_SECS` configures `checkout_timeout` (default five
seconds) for each reader-admission attempt. One operation can perform several
sequential reads, and each operation-scoped checkout has its own bounded wait;
the caller's total wall time can therefore exceed five seconds even though no
single admission wait does. The timeout never triggers a fresh connection open.

`KHIVE_READER_MAX_AGE_SECS` (default 300) and `KHIVE_READER_MAX_OPS` (default
5000) configure `reader_max_age` and `reader_max_ops`. A pooled reader connection
that has lived longer than the age limit, or has served more than the limit of
successful checkouts, is closed on return and replaced by a fresh connection before
the slot is reused; the replacement open happens inside the returning request's
tail. Both comparisons are strict, so a zero value for either key recycles on every
return. The in-memory pool shares one connection and is never recycled. Each replacement counts
in `reader_discards` on the acquisition snapshot, and a replacement that fails to
open counts in `reader_replacement_open_failures`.

### Synchronous `ReaderGuard::query_row`

A held `ReaderGuard` exposes one public SQL method, synchronous
`query_row(&self, sql, params, mapper)`. It retains the reader admission
classifier: writes, transaction control, and state-changing pragmas are refused
before SQLite execution. The mapper receives `khive_db::ReaderRow`, a borrowed
value-extraction view with `get` and `get_ref` by column index or name. It exposes
neither `rusqlite::Statement` nor a connection. Inferred mapper closures using
those methods remain compatible; explicit `&rusqlite::Row` annotations must use
`&ReaderRow<'_, '_>`.

Each call captures the current request context and observes its original
absolute deadline; later calls on the same lease do not renew that deadline.
An already cancelled or expired request never enters the mapper. Mapper entry is
gated on its own poll, so a cancellation arriving during parameter conversion,
which runs before any stepping, is refused there rather than after the mapper has
seen the row. SQLite VM stepping polls cancellation and the deadline through the
common progress-handler scope, and a stopped read returns
`SqliteError::RequestReadStopped(StorageError::Timeout { .. })`. Unrelated SQLite
and value-conversion failures retain their ordinary `SqliteError::Rusqlite`
variants, including a failure returned by the mapper after cancellation.

This is cooperative interruption. Synchronous Rust mapper code and native
callbacks cannot be forcibly preempted; after they return, a post-check refuses
a successful result if the request stopped. The method does not promise a hard
wall-time bound for such callbacks. Async pooled-reader progress checks retain
their lock-free path; only this synchronous borrowed path polls the request's
watch signal directly.

Callback cleanup and panic unwinding reuse the pooled-reader cleanup scope. A
cleanly interrupted or unwound lease remains usable. If cleanup fails, the held
lease refuses further queries; dropping it replaces a file-backed pooled reader
or retires the shared in-memory writer connection for the pool's lifetime.
Same-lease recursive calls from parameter conversion or mapper code are refused
before they can replace the outer query's progress handler. Calls using a
different lease and connection remain allowed. The in-progress guard clears on
ordinary return and panic unwinding.

### Closed standalone-reader exceptions

`ConnectionPool::open_standalone_reader` is crate-private and requires a
`StandaloneReaderPurpose`; there is no ordinary-request variant. The closed
list is:

- an explicitly requested, multi-call deferred raw-SQL read transaction;
- boot-time schema/model-registry inspection that runs before a runtime pool
  can own the read;
- a diagnostic that genuinely requires an independent snapshot.

The current PASSIVE `db_diagnostics` probe is not the third case: PASSIVE may
backfill WAL frames and therefore deliberately uses its separately documented,
untracked standalone writer. Adding any new standalone-reader purpose requires
changing the enum and the ADR exception list together.

For the raw-SQL transaction exception, `BEGIN`, `BEGIN TRANSACTION`, and
`BEGIN DEFERRED [TRANSACTION]` lazily open one standalone reader and retain one
reader-admission permit. Subsequent queries reuse that connection and permit;
`COMMIT`/`END` or full `ROLLBACK` closes the connection and releases admission
only after SQLite returns to autocommit. A successful begin also registers a
backend-scoped `sql_bridge_cached_read_transaction` span in `tx_registry`.
Immediate/exclusive starts, `START`, nested `BEGIN`, `SAVEPOINT`, `RELEASE`,
and `ROLLBACK ... TO` remain `StorageError::InvalidInput`. The compatible
standalone-open timeout remains `StorageError::Timeout` at
`sql_bridge.reader_open`; it is counted in reader diagnostics.

An interrupted explicit transaction is rolled back before reuse. If rollback
or callback cleanup cannot prove a safe autocommit connection, it is closed and
the logical reader is poisoned, preserving the existing
"connection already consumed" failure on later calls. A successful terminal
control closes the exceptional connection, and the same logical handle returns
to pooled routing for its next ordinary query.

### Reader diagnostics

The verb's existing root fields describe main. Its additive `databases` array has one entry
per canonical file (or distinct in-memory pool) opened by this serving runtime and its
pack handles, including events sidecars opened through runtime accessors after host
composition. Each entry has all alias
`backend_names`, canonical `path`, and either the same per-file
`diagnostics` field set or an `error` for that file. A
failed inspection does not suppress the other entries. This collector does not open any
unopened database path. Process checkpoint and audit counters are shared across entries;
reader and writer counters belong to each entry's representative pool. Diagnostics
holds late-opened pools only while collecting the report.

`db_diagnostics.reader_contention` is pool-scoped and resets only when the
`ConnectionPool` is reconstructed. It reports:

- `configured_reader_cap`, `configured_checkout_timeout_ms`, and
  `configured_busy_timeout_ms` from the pool's resolved configuration; the
  requested reader cap can exceed effective admission capacity when the pool
  degrades to one shared connection;
- `reader_admission_capacity` and the point-in-time
  `available_reader_admission_slots`;
- aggregate request `reader_acquisitions`, split into
  `pooled_reader_checkouts` and `standalone_reader_opens`;
- `infrastructure_standalone_reader_opens`, kept outside the request aggregate;
- `reader_checkout_timeouts`, including pooled and closed-exception admission
  waits but excluding cooperative cancellation;
- `reader_busy_timeouts`, queries on an already checked-out pooled reader
  (typed-store reads, pooled raw-SQL reads, and `ReaderGuard::query_row`) that
  SQLite refused with `SQLITE_BUSY` once `configured_busy_timeout_ms` elapsed.
  It is disjoint from `reader_checkout_timeouts`, so the two timeout classes
  can be told apart from the counters alone; writer refusals are not included;
- point-in-time and peak pooled checkouts, completed pooled checkouts, and the
  longest completed pooled hold in microseconds. Hold time includes reset or
  replacement because the connection is not reusable before that finishes.

These fields make a reader wait's shape observable end to end: capacity,
availability, active/peak work, bounded failures, and completed hold evidence
appear in one payload, so a caller can distinguish an admission wait that
resolves within its first window from one that times out. A flat
`standalone_reader_opens` counter across ordinary read verbs is ADR-166 G2's
route invariant.

### Writer handles and request cancellation

The manual `atomic_unit` path (write queue off or unavailable) and a standalone
`writer()` share a separate one-permit writer budget. A standalone read-write
writer preserves reads on its own connection for transaction visibility; each
reader-supertrait call also takes reader admission, but it is not a standalone
reader open. A queue-backed writer opens no standalone connection for ordinary
reads and uses the pool as described above; only its explicit deferred read
transaction takes the closed exception.

Request-owned reads install an exact-connection interrupt handle plus the
connection-global progress callback. Cancellation and the absolute deadline
stop SQLite VM work; the connection and permit remain owned until cleanup
finishes (or inside the detached worker after the bounded hard-cap path), so
admission cannot be returned ahead of live SQLite state. Prepared-statement
classification remains authoritative: only `Statement::readonly()` statements
outside transaction control register for interruption. DML-with-RETURNING,
transaction control, `execute_batch`, atomic units, and admitted writes preserve
their completion result.

`query_page` materializes at most the caller's requested `page.limit` rows but
does not impose a server-side maximum. Large offsets or expensive plans can
still make SQLite do substantial work before those bounded rows return.

## `execute_batch` transaction-control rejection and handle poisoning

Every `execute_batch` implementation rejects transaction-control statements
(`BEGIN`, `COMMIT`, `END`, `ROLLBACK`, `SAVEPOINT`, `RELEASE` — matched
case-insensitively at the statement head, tolerating leading whitespace and
`--`/`/* */` comments) with a typed `StorageError::InvalidInput` BEFORE
executing anything:

- The queue-backed path runs inside the writer task's own `BEGIN
  IMMEDIATE`; a caller `COMMIT` would close the task's transaction and
  terminate the writer task permanently.
- The standalone path wraps the caller list in its own `BEGIN IMMEDIATE`; a
  caller `COMMIT` would commit early, and a later statement failure would
  roll back only the tail — breaking all-or-nothing.
- The in-memory pool-backed path and the `InlineWriter` used by
  `atomic_unit` enforce the same rejection, since both run under an owned
  transaction boundary.

Rejected batches execute nothing, leave the handle untouched, and the handle
remains fully reusable.

Two further poisoning paths apply to the standalone (file-backed)
`execute_batch`, both dropping the handle instead of restoring it. Every
subsequent call on the poisoned handle then fails with the generic
"connection already consumed" pool error — callers must drop the handle and
acquire a fresh one, exactly as for cancellation invalidation above:

- **Failed ROLLBACK.** A statement failed and the error path's `ROLLBACK`
  also failed, leaving the connection's transaction state unknown. The
  returned error carries the poison context ("ROLLBACK after statement
  failure failed: ...") alongside the ORIGINAL statement error (never
  masked by the rollback failure alone). Pinned by
  `failed_rollback_poisons_handle_reuse_fails_loud`, which forces the
  failed ROLLBACK via a connection authorizer that denies the rollback
  transaction operation.
- **Non-transient BEGIN failure.** `BEGIN IMMEDIATE` failed with anything
  other than SQLite busy/locked (which is transient contention and restores
  the handle as reusable); the connection's transaction state is suspect.
  The returned error carries the poison context ("BEGIN IMMEDIATE failed
  non-transiently; connection transaction state is suspect") alongside the
  original BEGIN error. Pinned by
  `non_transient_begin_failure_poisons_handle` and
  `busy_begin_failure_restores_handle_reusable`.
- **COMMIT failure followed by successful ROLLBACK.** The transaction's
  `COMMIT` failed, cleanup `ROLLBACK` succeeded, and the handle is restored as
  reusable while the COMMIT error is surfaced to the caller. A failed
  ROLLBACK is the separate poisoning case above; the distinction keeps a
  recoverable connection available.

### `writer_task_handle_fails_loud_without_tokio_runtime`

ADR-067 Component A runtime-handle guard: `write_queue_enabled` is set but
the calling thread has no Tokio runtime context, so spawning the writer task
(which requires `tokio::spawn`) is impossible. `writer_task_handle` must
return a clean typed error instead of panicking.

Deliberately a plain `#[test]` (no Tokio runtime) — mirrors
`writer_task::spawn_fails_on_in_memory_pool`'s shape: the failure must be
observable without ever entering an async context, since entering one here
would defeat the point of the test.
