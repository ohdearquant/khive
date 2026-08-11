# ADR-091: Bounded read-transaction lifetime and WAL checkpoint escalation

**Status**: Accepted (ratified 2026-07-05)
**Date**: 2026-07-04
**Depends on**: ADR-015 (schema migrations), ADR-049 (daemon warm state)
**Fixes**: [#580](https://github.com/ohdearquant/khive/issues/580)

## Context

A long-lived reader can pin SQLite's WAL checkpoint boundary and cause the WAL file to grow
unbounded (#580). When several long-running processes hold open, otherwise-idle
connections against a database, the WAL file can grow to many times the size of the
database itself. Writes then start failing with `sqlite: invalid data:
timed out after 5s waiting for sqlite writer connection` on `comm.send` and other write ops.
The diagnostic signature is `PRAGMA wal_checkpoint(PASSIVE)` showing the writer not busy,
yet almost none of the accumulated WAL frames checkpointable. Closing the long-running idle
sessions — not merely leaving them idle — frees the WAL, which is the load-bearing datum
this ADR's mechanism has to explain: some per-process, long-lived state pins the checkpoint
boundary, and closing the process (not merely being idle) releases it.

A near-zero checkpointable count means SQLite's own checkpoint boundary, the oldest live
reader's mark, had barely moved despite the accumulated backlog. `PRAGMA
wal_checkpoint(PASSIVE)` never blocks and never reclaims past the oldest reader; it is by
design incapable of doing more than this once a reader pins the tail. The write timeouts are
a downstream symptom: as the WAL and `-shm` grow, `wal-index` operations degrade and both
readers and the writer do more work per statement, but the presenting error (`timed out ...
waiting for writer connection`) is a `khive-db` writer-mutex checkout timeout
(`crates/khive-db/src/pool.rs:308-319`), not a SQLite-level lock. `PASSIVE` reporting `busy=0`
means the writer mutex itself was not contended at diagnosis time; the writer was simply slow
underneath a bloated WAL, or hit timeouts during separate bursts. Root cause is squarely
"something pinned the checkpoint boundary," not writer contention.

### Correction: revised Plank 1 basis (this section replaces the original draft's basis)

Review of this ADR rejected the original mechanism on two points, both
confirmed correct against the code:

1. **An idle, returned autocommit connection does not pin a WAL snapshot.** The
   codebase's own regression test (`crates/khive-db/src/checkpoint.rs:404-441`) proves
   this directly: it must explicitly run `BEGIN DEFERRED; SELECT * FROM t;`
   (`checkpoint.rs:407-410`) to construct a pin, and its own doc comment states "An idle
   connection (no BEGIN) does NOT pin frames" (`checkpoint.rs:405-406`). SQLite's WAL
   documentation ties a reader's end mark to the duration of its transaction; an
   autocommit (implicit) read ends its transaction, and its snapshot lock, when the
   statement finishes and is reset. `conn.is_autocommit()` being `true` (the state the
   original draft flagged as ambiguous) in fact correctly indicates no held snapshot.
   The original Context section's claim that an idle pooled connection "carries its last
   snapshot forward indefinitely" is wrong and is retracted.
2. **Production reads never go through the pooled `ReaderGuard`/`return_reader` path the
   original Plank 1 targeted.** Grep-verified across every call site of
   `ConnectionPool::reader()` in the tree (`checkpoint.rs:407` is a test; all seven
   production call sites are inside each store's `with_reader` `else` branch: `graph.rs:
   107`, `vectors.rs:245`, `event.rs:101`, `note.rs:96`, `entity.rs:95`, `text.rs:126`,
   `sparse.rs:172`). Every one of those `else` branches is gated on `!self.is_file_backed`
   (see each store's `with_reader`, e.g. `entity.rs:81-99`, `vectors.rs:232-250`). Every
   production database is file-backed (`StorageBackend::sqlite`, `backend.rs:28-40`, sets
   `is_file_backed: true` unconditionally). So `pool.reader()` and, with it,
   `return_reader`'s recycling logic, is **dead code on the production read path** and
   only ever exercises for in-memory (test) backends. Recycling connections that
   production traffic never touches cannot fix a production incident.

Given both premises of the original Plank 1 are false for production, this revision
starts over: it inventories every place in the codebase that can actually hold a SQLite
read transaction open, states plainly which of those are proven safe by construction,
which are live-but-unlikely, and which cannot be resolved from static code reading alone
(and therefore get instrumented, not "fixed" on an unverified guess).

### Inventory: what can hold a WAL read mark in this codebase

**(1) Standalone per-call read connections: safe by construction, confirmed.** Every
production (file-backed) store's `with_reader` opens a fresh, standalone,
`SQLITE_OPEN_READ_ONLY` connection per call (`open_standalone_reader`, e.g.
`entity.rs:41-63`, mirrored in `note.rs`, `graph.rs`, `text.rs`, `vectors.rs`, `event.rs`,
`sparse.rs`), passes it into one `FnOnce(&Connection) -> Result<R, rusqlite::Error>`
closure executed synchronously inside `tokio::task::spawn_blocking`
(`entity.rs:88-91`, `vectors.rs:236-238`, same shape in every store), and drops the
connection when that closure returns (it is a function-local variable never escaped or
stored). The generic `SqlAccess` trait impl on `StorageBackend` (`backend.rs`, the
`reader()` method feeding `SqlBridge`) follows the identical open-standalone-per-call
pattern. `R` is always an owned value (`Option<SqlRow>`, `Vec<SqlRow>`, etc.); no call
site returns a live `Rows`/`Statement` cursor to the caller. A codebase-wide grep for a
struct field of type `Box<dyn SqlReader>` or `Box<dyn SqlTransaction>` (a long-lived
handle that could outlive one call) returned zero matches outside trait/return-type
declarations. **This read path is bounded to the wall-clock duration of one synchronous
closure and cannot explain a multi-hour pin**, unless that closure itself runs
pathologically long (see (3)).

**(2) `SqlBridge::begin_tx`'s explicit transactions: a genuine live-connection-duration
risk, but not demonstrated as the incident's cause, and NOT the only such risk (see
(2b)).** `sql_bridge.rs:848-894` opens a **standalone** connection and issues an explicit
`BEGIN` (`BEGIN DEFERRED` for read-only, `BEGIN IMMEDIATE` for read-write,
`BEGIN EXCLUSIVE` for serializable; `sql_bridge.rs:869-882`) that stays open, on that one
connection, for exactly as long as the caller holds the returned `SqliteTransaction`
before calling `commit()` or letting it drop. Tracing every call site of `begin_tx`
(`grep -rn "begin_tx(" crates`) finds exactly **one production caller**,
`khive-pack-session/src/mirror/ingest.rs:615`, plus test-only callers (including
`ingest.rs:2416`'s mid-transaction-error test, `khive-db/src/sql_bridge.rs:934`, and
`khive-db/src/backend.rs:754`), none reachable from production code. The one production caller uses
`SqlTxOptions::default()` (`read_only: false`, `SqlIsolation` not `Serializable`), which
resolves to `BEGIN IMMEDIATE`, a **write** transaction, not the read-only
`BEGIN DEFERRED` path. It is a bounded batch loop (one mirror-ingest pass over a file's
new events) that commits at the end of the function; it is not held across a poll-loop
sleep (`mirror/service.rs` sleeps at `service.rs:348` with no open transaction or
connection carried across that await; every tick reopens what it needs). The read-only
`BEGIN DEFERRED` branch requires either an explicit `SqlTxOptions { read_only: true, .. }`
caller (none exists in the tree today) or the entire backend opened via
`StorageBackend::sqlite_read_only` (`backend.rs:46-70`, an opt-in config path via
`cfg.read_only` in `serve.rs:1209`, not the default `khive.db` backend construction).
**This mechanism is real and worth bounding defensively, but it is a latent risk under
today's call graph, not a proven explanation for #580.**

**(2b) Raw `SqlWriter`-held transactions: a second, separate caller-controlled-duration
mechanism that bypasses `begin_tx` entirely (missed in the first revision, confirmed by
a full-workspace grep for `BEGIN (IMMEDIATE|DEFERRED|EXCLUSIVE)` across every crate).**
`begin_tx`/`SqliteTransaction` is not, in fact, "the one place in the codebase where
transaction duration is fully caller-controlled." A separate, more common pattern
acquires a plain `Box<dyn SqlWriter>` (via `sql.writer()`, either the standalone
file-backed writer or the pooled/in-memory writer) and issues `BEGIN IMMEDIATE`/`COMMIT`/
`ROLLBACK` as ordinary SQL statements through `execute`/`execute_batch`, entirely outside
`SqliteTransaction`'s tracking. Confirmed sites:

- `khive-pack-brain/src/fold_gate.rs:165-183` (`apply_fold_gate`): acquires a writer,
  issues raw `BEGIN IMMEDIATE`, runs the fold-gate dedup/mass-check/write, then `COMMIT`
  with a `ROLLBACK` fallback on failed commit. Its sibling
  `apply_fold_gate_and_append_event` (`fold_gate.rs:278-310`) issues its own
  `BEGIN IMMEDIATE`/`COMMIT` span and is a production path, called from the feedback
  handler (`khive-pack-brain/src/handlers.rs:1139`).
- `khive-db/src/pool.rs:175-181` (`WriterGuard::transaction`): a pooled-writer helper
  that issues `BEGIN IMMEDIATE`, runs the caller's closure, then commits or rolls back.
  Production callers include `khive-runtime/src/operations.rs:3610` (edge update) and
  the curation merge paths below. Because every `guard.transaction(...)` caller flows
  through this one helper, the helper itself is the instrumentation point; its callers
  need no per-site edits.
- `khive-pack-brain/src/persist.rs:330-400` (`persist_brain_state_mutation`): its own doc
  comment states this "deliberately does NOT use `SqlAccess::begin_tx`" because, per
  `fold_gate.rs`'s module doc, `begin_tx` "requires a file-backed database and errors for
  in-memory pools" used throughout this crate's test suite and by `KhiveRuntime::memory()`.
  This is a real architectural constraint, not an oversight: `begin_tx`'s standalone-
  connection design (`sql_bridge.rs:848-894`) has no in-memory-pool-compatible path today.
- `khive-db/src/sql_bridge.rs` itself: `SqliteWriter::execute_batch` (~340-380, standalone
  file-backed writer) and `PoolBackedWriter::execute_batch` (~715-745, pooled/in-memory
  writer) both issue raw `BEGIN IMMEDIATE`/`COMMIT`/`ROLLBACK` strings as part of their own
  batch-execution implementation, a second flavor of the same bypass.
- `khive-runtime/src/curation.rs` (`merge_entity`, ~270-300, 865, 1289): its doc comment
  states the whole merge (entity reads/writes, edge rewires, FTS updates, vec-index
  delete) "runs on a single pool connection inside one `BEGIN IMMEDIATE` transaction via
  `merge_entity_sql`." These spans flow through `WriterGuard::transaction` above, so
  instrumenting the helper covers them.
- Every store's own batch-upsert method: `entity.rs:325`, `text.rs:298/363/1111`,
  `note.rs:433`, `graph.rs:352`, `vectors.rs:356`, `event.rs:707/722`, `sparse.rs:249` each
  wrap a batch write in its own raw `BEGIN IMMEDIATE`/`COMMIT`.
- `khive-vcs/src/sync.rs:970-1010`: per-chunk entity and FTS-doc writes during KG
  sync/merge, each "one `BEGIN IMMEDIATE` / `COMMIT` per chunk," routed through the store
  batch methods above.

Every one of these is, today, a **short, function-scoped** batch (one fold-gate decision,
one brain-state mutation, one entity merge, one chunk of a sync). None is demonstrated to
be held across an await or a multi-hour span. But the same category of risk that
motivates bounding `begin_tx` applies here: nothing currently prevents a future change
(an error path that returns before `COMMIT`/`ROLLBACK`, a batch loop that grows unbounded,
a nested call that holds the writer across an external call) from turning one of these
into a long-held write transaction. Since production traffic overwhelmingly goes through
this pattern rather than `begin_tx`, **excluding it from Plank 1 would leave the
instrumentation and caps blind to the majority of the codebase's actual caller-controlled
transaction surface.**

**(3) A pathologically long single closure inside (1).** Because (1)'s connections are
provably bounded to the closure's own execution, an ANN/vector search, graph traversal,
or bulk export that itself runs for a very long time while holding its standalone reader
would still pin the tail for that duration. This is self-terminating (the request
eventually returns), which sits awkwardly against the incident's evidence of >24h-old
_processes_ mattering, but cannot be fully ruled out for pathological queries (e.g. an
unbounded `traverse` or a brute-force ANN fallback over a large corpus).

**(4) The `vec0` (sqlite-vec) virtual table's internal cursor/transaction semantics.**
`vectors.rs` queries `vec0` tables through the same bounded standalone-connection pattern
as (1), so from the Rust wrapper's perspective KNN queries are bounded the same way.
`vec0` itself, however, is a loaded native extension (`extension.rs`) whose own internal
locking/cursor behavior during a KNN scan is not visible from this repository's Rust
source and was **not verified** in this review. This is flagged as an open question, not
asserted as a cause.

**(5) The pool's own eagerly-opened, permanently idle reader connections.** For
completeness: `ConnectionPool::new` (`pool.rs:221-243`) always opens `max_readers`
(default up to 8) pooled reader connections at construction, even for file-backed
backends whose reads never route through them per the finding above. These sit open for
the process lifetime, but a WAL snapshot begins with a connection's _first statement_,
not at `open()` (no PRAGMA in `configure_reader_connection`, `pool.rs:534-540`, executes
a `SELECT` against the schema). Since these connections never execute a statement in
file-backed production mode (nothing calls `pool.reader()` there), they never take a
snapshot and are **not a candidate**.

### Honest conclusion

Static code reading does not conclusively identify a Rust-level mechanism that holds a
read transaction open for the incident's observed timescale (processes live >24h). The
strongest remaining candidates, in order of plausibility, are: (2)/(2b) if a future or
missed caller ever holds a `begin_tx` or raw-`SqlWriter` transaction across a long idle
span (not currently demonstrated for either), (4) `vec0`'s internal behavior (unverified,
native code, needs targeted instrumentation or upstream documentation review), and (3) a
pathologically long bounded query (self-terminating, doesn't match the ">24h idle
process" shape well). Per
讲事实摆道理: rather than assert an unproven mechanism and design enforcement around it,
this ADR now leads with instrumentation to let production telemetry identify the actual
pin source before tuning any enforcement threshold, and separately bounds the one
mechanism (2) that is real, live, and caller-controllable, even though it isn't proven to
be this incident's specific trigger.

### Non-goals

This ADR does not redesign writer serialization (the single-writer-mutex model is
unchanged), does not change journal mode away from WAL, and does not speculate further
about `vec0`'s internal C implementation beyond flagging it as unverified. Batch-write
contention and multi-writer scaling are tracked separately.

## Decision

Three parts. Plank 0 instruments the checkpoint task to name what is actually pinning
the boundary in production, since static reading could not conclusively identify it.
Plank 1 bounds every mechanism proven to allow caller-controlled transaction duration:
`begin_tx` (2) **and** raw `SqlWriter`-held transactions (2b), via one shared tracking
mechanism, plus the in-memory/test pooled-reader path the original draft targeted,
narrowed to the surface it actually covers. Plank 2 (TRUNCATE escalation) carries over
from the original draft largely unchanged, with an explicit flap/backoff statement added
by design review.

**Migrate-vs-instrument decision for (2b):** this ADR does **not** propose migrating the
raw-`SqlWriter` call sites (`fold_gate.rs`, `persist.rs`, `sql_bridge.rs`'s own writer
impls, `curation.rs`, every store's batch methods, `khive-vcs/src/sync.rs`) onto
`begin_tx`. `persist.rs`'s own doc comment names a real constraint: `begin_tx`'s
standalone-connection design has no in-memory-pool-compatible path, and in-memory pools
are load-bearing for this crate's test suite and for `KhiveRuntime::memory()`. Migrating
would mean either breaking that test-pool compatibility or first building a pooled
variant of `begin_tx`, both larger and riskier than the WAL-pinning problem this ADR is
fixing. Instead, Plank 1 extends the same age-tracking/enforcement mechanism to cover
raw `SqlWriter` transactions **in place**, via a small shared open-transaction registry
that both `SqliteTransaction::begin_tx` and the raw-BEGIN call sites register with. This
keeps each call site's existing connection-acquisition strategy (standalone vs. pooled,
file-backed vs. in-memory) untouched and adds only a `register`/`deregister` pair around
each existing `BEGIN`/`COMMIT`-or-`ROLLBACK` span.

### Plank 0: instrumentation before enforcement tuning

Because Plank 1's thresholds cannot be responsibly chosen without knowing which
mechanism is real, add observability first and treat it as a prerequisite deliverable,
not an optional nice-to-have:

- On every `run_checkpoint_task` tick (`checkpoint.rs:141-183`), in addition to the
  existing `wal_pages` observation, log (`tracing::debug!` normally, escalating to
  `tracing::warn!` once `wal_pages` crosses `warn_pages`, matching the existing
  rate-limited crossing pattern) the age of the oldest currently-open transaction in the
  shared open-transaction registry (Plank 1, covering both `begin_tx` and raw
  `SqlWriter`-held transactions), if any, and the current WAL frame count.
- On a TRUNCATE attempt (Plank 2) that fails to make progress (`wal_pages_after` within a
  small epsilon of `wal_pages_before`), enumerate and log every currently-open registry
  entry's start time, elapsed duration, and (if the caller supplied one) a label, reusing
  the **existing** `label: Option<String>` field already present on both `SqlTxOptions`
  and `SqlStatement` (`khive-storage/src/types/sql.rs:66-69`; no schema/type change
  needed, e.g. `ingest.rs`'s `label: Some("session_mirror_insert_message")` pattern, and a
  new label passed at each raw-`SqlWriter` call site, e.g.
  `label: Some("fold_gate_apply")`, `label: Some("brain_persist_mutation")`,
  `label: Some("merge_entity")`, `label: Some("entity_upsert_batch")`). This directly
  answers the question this ADR could not answer from static reading: which specific
  caller, if any, is holding the pin, the next time this happens in production.
- This data gates Plank 1's threshold tuning: `KHIVE_TX_MAX_AGE_SECS` (below) ships with
  a conservative default and is explicitly called out as provisional pending one cycle
  of production telemetry from this plank.

### Plank 1: bound every caller-controllable transaction path via a shared registry, retarget the rest

**Shared open-transaction registry (new, covers both `begin_tx` and raw `SqlWriter`
transactions).** A process-wide registry (a `Mutex<HashMap<TxId, TxMeta>>` or equivalent;
`TxMeta { opened_at: Instant, label: Option<String> }`) is the single place both
mechanisms register:

- `SqliteTransaction::begin_tx` (`sql_bridge.rs:848-894`) registers on `BEGIN`
  (`sql_bridge.rs:882-883`) and deregisters on `commit()`/`drop`.
- Each raw-`SqlWriter` transaction span identified in Inventory (2b) (`fold_gate.rs`'s
  `apply_fold_gate` and `apply_fold_gate_and_append_event`, `persist.rs:330-400`,
  `sql_bridge.rs`'s `SqliteWriter`/`PoolBackedWriter::execute_batch`,
  `pool.rs`'s `WriterGuard::transaction` — one instrumentation point covering all
  `guard.transaction(...)` callers, including `curation.rs`'s merge paths and
  `operations.rs:3610`'s edge update — every store's batch-upsert method, and
  `khive-vcs/sync.rs`'s per-chunk writes) wraps its existing
  `BEGIN IMMEDIATE` / `COMMIT`-or-`ROLLBACK` span with
  a `register(label)` call immediately after `BEGIN` succeeds and a `deregister(id)` call
  in both the commit and rollback paths (including error paths that currently return
  before reaching `COMMIT`, which this change forces to be explicit about). This is
  additive at each site: it does not change connection acquisition, isolation level, or
  commit/rollback logic, only adds a bookkeeping call around the existing span.

Two enforcement points read the registry, applied uniformly to every registered
transaction regardless of which mechanism created it:

- **Soft cap (logging only):** on every `execute`/`query_row`/`query_all` call routed
  through a registered `SqliteTransaction`, and on every checkpoint tick (Plank 0) for
  raw-`SqlWriter` entries (which have no per-statement hook to piggyback on), check the
  registry entry's `opened_at.elapsed()` and log a rate-limited `tracing::warn!` (same
  edge-triggered pattern as `crossing_warn`, `checkpoint.rs:224-228`) once it exceeds
  `KHIVE_TX_WARN_SECS` (default **30s**; provisional, see Plank 0), including the entry's
  `label` if supplied.
- **Cooperative stale-operation guard, not a lifetime bound (reworded:
  the original "hard cap" language overclaimed).** Once a registry entry's
  `opened_at.elapsed()` exceeds `KHIVE_TX_MAX_AGE_SECS` (default **120s**; provisional, see
  Plank 0):
  - **SUPERSEDED (see the 2026-07-12 amendment at the end of this ADR) — historical design
    intent, not shipped behavior.** The three sub-bullets immediately below (per-statement
    reject, `commit()`-past-cap rollback, and their raw-`SqlWriter` mirror) targeted
    `SqliteTransaction`/`begin_tx`, an API this codebase no longer has: ADR-067's
    `atomic_unit` closure replaced every production write-transaction path with a span that
    structurally cannot outlive its own call, which is exactly the "closure-scoped
    transaction API" follow-up named two paragraphs below — already delivered, for writes,
    by a later ADR. What actually shipped is the fourth sub-bullet only (the background
    registry sweep), generalized to run independently of `run_checkpoint_task`'s
    Observed/Skipped WAL-checkpoint outcome and to cover every registered span, not only
    `SqliteTransaction`/raw-`SqlWriter` sites. No reject-on-statement or rollback-on-commit
    mechanism exists anywhere in the shipped code; a stale span is surfaced, never
    force-closed. Kept verbatim below for the historical record of what this ADR originally
    specified.
  - For `SqliteTransaction`: subsequent `execute`/`query_row`/`query_all` calls on that
    transaction return an error instead of running the statement, forcing the caller's own
    error-handling path to abort and drop the transaction. This is a **guard against a
    caller that keeps issuing statements past the cap**, not a bound on how long an
    already-open, currently-idle transaction can sit un-acted-upon: a transaction that
    opens, runs one statement, and is then held across a long await with no further
    `execute`/`query_row`/`query_all` call never trips this check, because there is no
    subsequent call for it to intercept. Fixing that gap requires either (a) an active
    background sweep of the registry that force-drops entries past a harder ceiling
    (deferred, see below) or (b) the closure-scoped transaction API (see Plank 1's
    follow-up note) that makes "held past the return of an async function" structurally
    impossible. This ADR ships (a) as an explicit, separate mechanism rather than folding
    it into the per-statement check:
    - **`commit()` past the cap:** `SqliteTransaction::commit()` checks `opened_at.elapsed()`
      before issuing `COMMIT`; past `KHIVE_TX_MAX_AGE_SECS` it issues `ROLLBACK` instead and
      returns an error to the caller, rather than silently committing a transaction that
      has already been flagged as stale. This closes the previously unspecified
      "`commit()` after the cap" gap: legitimate long-running batches that hit this will
      have their work rolled back and must retry in smaller chunks (see Failure modes).
    - **Background registry sweep (Plank 0's checkpoint tick, extended) — this sub-bullet is
      the part that shipped, generalized (2026-07-12) to run on every tick regardless of
      Observed/Skipped:** any registry entry whose `opened_at.elapsed()` exceeds
      `KHIVE_TX_MAX_AGE_SECS` is logged (`tracing::warn!` past `KHIVE_TX_WARN_SECS`,
      `tracing::error!` past `KHIVE_TX_MAX_AGE_SECS` — escalating in severity the longer it
      persists) even if the owning caller never issues another statement or calls
      `commit()`. This does **not** force-close the connection (that would require unsafe
      cross-thread manipulation of a connection another task owns); it makes a stuck
      transaction visible to an operator via the checkpoint tick's existing log line, the
      same visibility-over-guaranteed-reclamation posture Plank 2 takes for sustained
      TRUNCATE failure (see the severity ladder amendment below).
  - For raw `SqlWriter` sites, the same `commit()`-past-cap and background-sweep behavior
    apply at the registry level; each site's existing commit call is wrapped to check the
    registry entry's age before issuing `COMMIT` and to `ROLLBACK` instead past the cap,
    matching `SqliteTransaction`'s behavior.
- `KHIVE_TX_WARN_SECS` / `KHIVE_TX_MAX_AGE_SECS` are deliberately generous relative to
  every known production caller (the one bounded `begin_tx` mirror-ingest batch, and the
  (2b) sites' function-scoped fold-gate/persist/merge/batch-upsert spans, all expected to
  complete in well under a second in normal operation) so this guard is a safety net for a
  runaway loop or a future caller, not a routine limit.
- **Follow-up, not designed here:** a closure-scoped transaction API (`with_tx(|tx| { ...
  })` that structurally cannot outlive the closure, eliminating the "held across an await"
  class of risk entirely) is named as a candidate for a future ADR, once Plank 0's
  telemetry shows whether this class of bug actually occurs in practice. This ADR does not
  design it now.

**Pooled `ReaderGuard` recycling: keep, narrow the claim.** The original draft's
age/op-count recycling on `return_reader` (`pool.rs:434-454`) is retained exactly as
designed, because it is harmless and still correct hygiene, but the ADR no longer claims
it protects production file-backed traffic: it only ever executes for in-memory/test
`ConnectionPool` instances (see the correction above). State this explicitly so a
future reader of this ADR does not re-inherit the false production claim.
`KHIVE_READER_MAX_AGE_SECS` (default 300s) and `KHIVE_READER_MAX_OPS` (default 5000)
config keys are retained under this narrowed scope.

**Checkout-age watchdog: retained, same narrowed scope.** `oldest_checkout_age()`
(as originally specified) is still useful for the in-memory/test pool path and for any
future production caller of `pool.reader()`, so it is kept, but is not claimed to cover
today's production reads.

### Plank 2: daemon-side TRUNCATE escalation (carried over, with explicit backoff)

Unchanged from the original draft in mechanism: the periodic task keeps PASSIVE-only,
`try_writer_nowait`, skip-on-busy behavior for every ordinary tick
(`checkpoint.rs:196-214`); this plank adds a separate, much rarer escalation path.

- `CheckpointConfig` gains `truncate_high_water_pages` (default **20,000 pages**,
  `KHIVE_WAL_TRUNCATE_HIGH_WATER_PAGES`), `truncate_min_interval` (default **5 minutes**,
  `KHIVE_WAL_TRUNCATE_MIN_INTERVAL_SECS`), and `truncate_busy_timeout` (default
  **2000ms**, `KHIVE_WAL_TRUNCATE_BUSY_MS`), with the same semantics as originally
  specified: past the high-water mark, no more often than the min interval, attempt
  `PRAGMA wal_checkpoint(TRUNCATE)` via `try_writer_nowait` with a temporarily shortened
  busy timeout restored immediately after, win or lose.
- **Explicit flap/backoff behavior:** if `try_writer_nowait()` itself
  fails (the writer mutex is held by a concurrent write) at the moment a TRUNCATE attempt
  is due, the attempt is skipped for that tick exactly like an ordinary PASSIVE skip; the
  task does not retry within the same tick or spin-wait. `last_truncate_attempt` is
  **not** updated on a skip (only on an attempt that actually acquired the writer), so
  the next tick where the writer is free is eligible immediately rather than waiting out
  the full `truncate_min_interval` again. **One writer checkout per tick** (matching the
  current loop shape, `checkpoint.rs:196-214`): if `try_writer_nowait()` fails, both the
  PASSIVE observation and any due TRUNCATE are skipped for that tick; if it succeeds, the
  tick runs PASSIVE first and then, if due, TRUNCATE under that same guard. **Accepted worst case, stated explicitly:** if the writer is continuously busy
  for the entire observation window, TRUNCATE never runs and the WAL keeps growing past
  `truncate_high_water_pages`. Visibility, not guaranteed reclamation, is the mitigation
  (see the severity ladder below): sustained pressure surfaces via the WARN tier (a
  configurable drain-failure counter across consecutive observed checkpoint cycles at
  `warn_pages`, default N=3) and, once `truncate_high_water_pages` is crossed, the shipped
  ALARM/TRUNCATE-escalation tier in this plank, rather than promising
  unconditional reclamation, which would require blocking writer acquisition (rejected,
  see original Alternatives).
- Observability: unchanged from the original draft (`tracing::info!` per attempt with
  before/after page counts and elapsed time; `tracing::warn!` after three consecutive
  attempts fail to clear `warn_pages`), extended per Plank 0 to also log every open entry
  in the shared transaction registry (both `begin_tx` and raw `SqlWriter` transactions)
  when an attempt fails to make progress.

### 2026-07-04 amendment: severity ladder + `wal_pages` units

**Severity ladder (this corrects Plank 0's crossing-severity wording above).** Plank 0's
former description of the `warn_pages` crossing (`escalating to tracing::warn! once
wal_pages crosses warn_pages`) is superseded: crossing `warn_pages` (default 2000,
`KHIVE_WAL_WARN_PAGES`) on its own is **INFO**, not WARN, because it is an expected,
self-resolving event under ordinary write bursts, not an operator-actionable condition.
The ladder is:

- **INFO**: `wal_pages` crosses `warn_pages` (a single tick observation).
- **WARN**: `wal_pages` fails to drain back below `warn_pages` across **N = 3** consecutive
  checkpoint cycles (each cycle is one `run_checkpoint_task` tick, default 500ms via
  `KHIVE_CHECKPOINT_INTERVAL_MS`). This tier is implemented. N is configurable with the
  positive-integer `KHIVE_WAL_WARN_SUSTAINED_CYCLES` setting (default `3`; unset,
  unparseable, zero, or a value outside `u8` falls back silently). The state machine counts
  consecutive **observed** ticks at or above `warn_pages`, emits WARN once per elevation
  episode, and rearms only after an observed tick below `warn_pages`; a skipped writer-busy
  tick neither increments nor resets the episode. It remains distinct from
  `note_truncate_outcome`, which counts consecutive TRUNCATE _attempts_, not ordinary
  checkpoint cycles, and only runs once `wal_pages` has crossed the much higher
  `truncate_high_water_pages` threshold and a rate-limited TRUNCATE was actually attempted.
- **ALARM**: the Plank 2 TRUNCATE-escalation tier, armed by `truncate_high_water_pages`
  (default 20000, `KHIVE_WAL_TRUNCATE_HIGH_WATER_PAGES`, "a separate, much higher threshold
  than `high_water_pages`", `checkpoint.rs:198-219`) via `maybe_truncate`
  (`checkpoint.rs:1598-1690`). Crossing `high_water_pages` (default 6000,
  `KHIVE_WAL_HIGH_WATER_PAGES`, logged at `checkpoint.rs:1384-1393`) remains a shipped
  intermediate log between the WARN and ALARM tiers, but it is not itself a ladder tier: it
  neither arms nor performs any TRUNCATE attempt, and must not be conflated with the
  `truncate_high_water_pages` crossing that actually does.

**Implementation evidence.** `CheckpointConfig`, its environment parsing, the
episode-scoped state machine, and the INFO/WARN log calls are in
`crates/khive-db/src/checkpoint.rs:156-185,248-293,404-510,1345-1382`. Regressions pin the
first-crossing INFO, third-consecutive-cycle WARN, one-shot/rearm behavior, isolated
crossings, and valid/zero/invalid environment values at
`crates/khive-db/src/checkpoint.rs:3308-3435,3923-3950`.

**Residual work.** The sustained WARN is observability only: it does not force a
checkpoint, identify or release the pinning reader, or bypass the separate
`truncate_high_water_pages` and `truncate_min_interval` gates. Reader attribution and
TRUNCATE escalation therefore remain separate operational mechanisms; no further
severity-ladder implementation is pending.

**Units: `wal_pages` is an instantaneous frame count, not a cumulative counter.**
`query_wal_pages` (`checkpoint.rs:545-561`) reads it from `PRAGMA wal_checkpoint`'s
3-column row `(busy, log, checkpointed)`: `log` (column index 1) is the number of frames
currently sitting in the WAL file at the moment of the call, not frames accumulated over
time. A frame is one page (khive.db's page size is SQLite's unconfigured default, 4096
bytes; no `PRAGMA page_size` override exists in `pool.rs`'s connection setup) plus a
24-byte WAL frame header. The pragma's own side effect (a PASSIVE checkpoint) means two
consecutive calls can observe a falling count with no explicit checkpoint in between.

Separately, the WAL file's _resting_ on-disk size is capped by the pool's
`journal_size_limit_bytes` (`pool.rs:44-49`, default 64MiB,
`DEFAULT_JOURNAL_SIZE_LIMIT_BYTES = 67_108_864`, overridable via
`KHIVE_JOURNAL_SIZE_LIMIT_BYTES`, `pool.rs:85`): SQLite truncates the WAL file back down
after a log-resetting (TRUNCATE-mode) checkpoint, which is exactly the mechanism
`maybe_truncate` (`checkpoint.rs:428-506`) invokes. `wal_pages` and
`journal_size_limit_bytes` are not the same quantity: one is a live frame count sampled per
tick, the other is a byte ceiling enforced only at TRUNCATE time, and this ADR's
thresholds (`warn_pages`, `high_water_pages`, `truncate_high_water_pages`) are all
expressed in the former, page-count, unit.

### Config summary

| Key                                    | Default | Plank | Purpose                                                                                                                                                                        | Status                                          |
| -------------------------------------- | ------- | ----- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ----------------------------------------------- |
| `KHIVE_TX_WARN_SECS`                   | 30      | 1     | Background sweep: `tracing::warn!` once the shared registry's oldest entry's age exceeds this cap (any `khive_storage::tx_registry`-registered span, logging only)             | Implemented, adapted — see 2026-07-12 amendment |
| `KHIVE_TX_MAX_AGE_SECS`                | 120     | 1     | Background sweep: `tracing::error!` once the same entry's age exceeds this cap (logging only — no per-statement reject or `commit()` rollback ships; see 2026-07-12 amendment) | Implemented, adapted — see 2026-07-12 amendment |
| `KHIVE_READER_MAX_AGE_SECS`            | 300     | 1     | Recycle a pooled reader connection past this age on return (in-memory/test pool only)                                                                                          | Carried over, scope narrowed                    |
| `KHIVE_READER_MAX_OPS`                 | 5000    | 1     | Recycle a pooled reader connection past this op count on return (in-memory/test pool only)                                                                                     | Carried over, scope narrowed                    |
| `KHIVE_READER_CHECKOUT_WARN_SECS`      | 10      | 1     | WARN when the oldest outstanding pooled checkout exceeds this age (in-memory/test pool only)                                                                                   | Carried over, scope narrowed                    |
| `KHIVE_WAL_WARN_SUSTAINED_CYCLES`      | 3       | 0     | Consecutive observed ticks at/above `warn_pages` before the one-shot per-episode WARN; invalid or zero values fall back silently                                               | Implemented                                     |
| `KHIVE_WAL_TRUNCATE_HIGH_WATER_PAGES`  | 20000   | 2     | WAL page count that arms a TRUNCATE attempt                                                                                                                                    | Carried over                                    |
| `KHIVE_WAL_TRUNCATE_MIN_INTERVAL_SECS` | 300     | 2     | Minimum spacing between successful TRUNCATE attempts                                                                                                                           | Carried over                                    |
| `KHIVE_WAL_TRUNCATE_BUSY_MS`           | 2000    | 2     | Temporary busy_timeout override during a TRUNCATE attempt                                                                                                                      | Carried over                                    |

Existing, unchanged: `KHIVE_CHECKPOINT_INTERVAL_MS` (500), `KHIVE_WAL_WARN_PAGES` (2000),
`KHIVE_WAL_HIGH_WATER_PAGES` (6000), `KHIVE_JOURNAL_SIZE_LIMIT_BYTES` (64MiB),
`KHIVE_BUSY_TIMEOUT_SECS` (30), `KHIVE_CHECKOUT_TIMEOUT_SECS` (5).

## Alternatives considered

1. **WAL2** (upstream SQLite's two-rotating-WAL-file mode). Rejected: not shipped in the
   stable `rusqlite`/bundled `libsqlite3` version khive depends on; adopting it would
   mean vendoring a patched SQLite build for a config-and-scheduling-level fix. Revisit
   only if WAL2 reaches upstream stable and the config-level fix proves insufficient.
2. **External checkpointer process** (litestream-style out-of-process WAL manager).
   Rejected: khive embeds SQLite in-process by design (single self-contained binary); an external
   process reintroduces an operational dependency and IPC surface for a problem a
   background `tokio::spawn` task (already present, `checkpoint.rs`) solves in-process.
3. **Kill long-lived reader sessions at the OS level** (SIGKILL `kkernel mcp` processes
   older than N hours). Rejected: violent, drops in-flight agent work, and does not fix
   the mechanism since a freshly started session can re-pin the tail immediately.
   Long-lived stdio sessions are live Claude Code instances; killing them by policy is a
   worse user experience than bounding transaction/connection lifetime underneath them.
   Also notable: freeing the WAL by killing idle processes is exactly this alternative
   applied manually, which is precisely why it is not an acceptable long-term policy
   rather than evidence the mechanism is understood.
4. **Route all reads through the daemon instead of per-process pools** (collapse "N
   independent `ConnectionPool`s against one file" into one daemon-mediated reader path).
   Would remove the multi-process topology entirely and is a natural extension of
   ADR-049's daemon warm-state model. Noted as a future direction, out of scope here: it
   requires an MCP transport change (stdio sessions proxying reads through the daemon
   socket) beyond a bounded-lifetime fix, and per this ADR's inventory, production reads
   are already bounded per-call, so this alternative would not by itself have prevented
   #580 unless the actual pin turns out to be a `begin_tx` or raw-`SqlWriter` write path
   that a daemon-mediated design would also need to serialize correctly.

## Failure modes

- **SUPERSEDED — historical, not a real failure mode of the shipped code (see the
  2026-07-12 amendment).** The following two bullets described failure modes of the
  per-statement reject / `commit()`-rollback enforcement Plank 1 originally specified
  against `SqliteTransaction`/`begin_tx`. That API and that enforcement do not exist in
  the shipped code; kept for the historical record, not as a description of current
  behavior. The failure mode that actually applies to what shipped is the THIRD bullet
  below ("no in-process mechanism can force-close a stale span"), which now covers every
  registered span, not only an idle-between-statements one.
- **Stale-op guard rejection / commit-time rollback during a legitimate long-running
  batch.** If a future caller (via `begin_tx` or a raw `SqlWriter` transaction)
  legitimately needs a transaction open longer than `KHIVE_TX_MAX_AGE_SECS` (120s
  default), the guard forces its next `execute`/`query_row`/`query_all` call to fail, and
  a `commit()` past the cap is rolled back instead of committed, forcing the caller to
  retry in smaller batches. This is an intentional trade: no code path in the tree today
  needs a transaction anywhere near that long (the one `begin_tx` mirror-ingest batch and
  the (2b) raw-`SqlWriter` sites are all bounded per-file/per-chunk/per-mutation spans
  expected to complete well under a second in normal operation); if this becomes a real
  constraint, raise the cap rather than remove it, using Plank 0's telemetry to confirm
  the caller's real needs first.
- **Idle-held transaction between statements, never re-checked.** As stated in Plank 1,
  the per-statement guard cannot catch a transaction that opens, runs one statement, and
  is then held idle (no further `execute`/`query_row`/`query_all`, no `commit()`) across a
  long await. The background registry sweep (Plank 0's checkpoint tick) surfaces this via
  a WARN, but does not force-close the connection. This is an accepted gap in this ADR's
  first iteration, not a silent one: the closure-scoped transaction API follow-up (Plank 1)
  is the structural fix, deferred pending Plank 0 telemetry showing whether this actually
  occurs.
- **No in-process mechanism force-closes any stale span (what actually shipped).** Every
  span registered in `khive_storage::tx_registry` — not only an idle-between-statements
  one, since no per-statement or per-commit check exists at all — gets a `warn!` past
  `KHIVE_TX_WARN_SECS` and an `error!` past `KHIVE_TX_MAX_AGE_SECS` from the background
  sweep (every `run_checkpoint_task` tick, Observed or Skipped) and nothing more: no
  reject, no rollback, no kill. This is the accepted gap this ADR's first shipped
  iteration lands on. ADR-067's `atomic_unit` already eliminates the "held past the
  return of an async function" class of risk for every production write path, which is
  most of what the deferred closure-scoped-API follow-up would have targeted. Before
  Amendment 4, the remaining unbounded span was `graph.rs`'s chunked-traversal read
  snapshot (`graph_traverse_read`); Amendment 4 replaces it with bounded,
  statement-scoped spans. The remaining class is any future caller of a
  registry-registered span this ADR did not anticipate.
- **TRUNCATE contention**: bounded to `truncate_busy_timeout` (default 2s) per attempt,
  at most once per `truncate_min_interval` under normal conditions (see the flap/backoff
  note: a skipped attempt due to writer contention does not consume the interval).
- **Flap under sustained writer load**: per the explicit backoff statement above, if the
  writer is continuously busy, TRUNCATE never fires and WAL growth continues past
  `truncate_high_water_pages`; visibility via the severity ladder (see the 2026-07-04
  amendment: the implemented WARN drain-failure tier and the shipped ALARM/TRUNCATE tier)
  is the accepted mitigation, not unconditional reclamation.
- **Instrumentation overhead**: Plank 0's per-tick age check and per-attempt transaction
  enumeration are cheap (in-process counters/timestamps, no extra SQL queries beyond
  what TRUNCATE failure logging already requires) and do not change checkpoint task
  timing in any way that matters at a 500ms tick interval.
- **Pooled reader recycling failure modes**: unchanged from the original draft, but now
  understood to apply only to the in-memory/test pool path; any behavior change there
  has no production blast radius.

## Consequences

- The false premise from the original draft (idle pooled readers pin production WAL) is
  retracted; this ADR no longer claims a fix for a mechanism that does not exist in the
  production code path.
- WAL growth now has a visibility sweep (SUPERSEDED description below — see the
  2026-07-12 amendment) covering every caller-controllable transaction mechanism this
  review confirmed exists: `begin_tx`'s `SqliteTransaction` **and** the raw
  `SqlWriter`-held transactions in `khive-pack-brain` (`fold_gate.rs`, `persist.rs`),
  `sql_bridge.rs`'s own writer implementations, `curation.rs`'s `merge_entity`, every
  store's batch-upsert method, and `khive-vcs`'s chunked sync writes, all sharing one
  open-transaction registry (in practice, today, `atomic_unit`'s registered span for
  every production write path). The shipped guard **escalates a stale span to
  `tracing::warn!`/`error!` (background sweep, every checkpoint tick); it does not reject
  statements, roll back a `commit()`, or force-close a connection held idle across an
  await with no further calls** — visibility only, an accepted gap tracked as a
  follow-up (see Failure modes). The originally-specified per-statement reject and
  commit-time rollback against `SqliteTransaction`/`begin_tx` were never built against
  that API before ADR-067's `atomic_unit` superseded it for writes.
- Plank 0's instrumentation is the load-bearing deliverable of this ADR's first
  iteration: it converts "we don't know what's pinning the WAL" into a concrete,
  loggable answer the next time sustained WAL pressure occurs, which Plank 1's
  provisional thresholds and any follow-up ADR amendment can then be tuned against.
- The existing periodic PASSIVE checkpoint tick and its skip-on-busy behavior are
  unchanged; TRUNCATE escalation is additive to `checkpoint.rs`, not a rewrite, with an
  explicit accepted-worst-case statement for sustained writer contention. The severity of
  the `warn_pages` crossing itself is amended (see "2026-07-04 amendment" above): crossing
  is INFO, the implemented and configurable WARN is reserved for an N-consecutive-observed-
  cycle drain failure (default 3), and `truncate_high_water_pages` arming the TRUNCATE
  escalation is the ALARM tier. `high_water_pages` crossing remains a shipped intermediate
  log, not a ladder tier on its own. The WARN remains diagnostic only; attribution and
  remediation are outside that rung.
- Two new config knobs for the shared transaction-registry sweep (Plank 1), covering
  every `khive_storage::tx_registry`-registered span — `begin_tx`'s historical
  `SqliteTransaction` target no longer exists; the real coverage today is `atomic_unit`'s
  registered span for every production write path, plus `graph.rs`'s bounded,
  statement-scoped traversal reads after Amendment 4 — three carried-over knobs
  narrowed in scope, three for TRUNCATE
  escalation (Plank 2); the two new keys are explicitly marked provisional pending one
  cycle of production telemetry rather than presented as tuned defaults.
- `SqlTxOptions`/`SqlStatement`'s existing `label: Option<String>` field
  (`khive-storage/src/types/sql.rs:66-69`) is reused for registry entries; no new field or
  schema change is introduced by this ADR.
- Follow-up (tracked separately, not blocking this ADR): once Plank 0 telemetry
  identifies whether `vec0`'s internal cursor behavior, a missed `begin_tx`/raw-`SqlWriter`
  caller, or something else entirely is the actual #580 mechanism, file a short ADR
  amendment narrowing or retuning Plank 1 rather than re-guessing from static code reading
  again. The closure-scoped transaction API (Plank 1's named follow-up) is also tracked
  here as a candidate future ADR.

### 2026-07-12 amendment: Plank 1 implemented as a background age sweep, not per-statement rejection

Live incident (2026-07-12): the daemon logged `WAL high-water mark exceeded; sustained WAL
pressure — a long-lived reader may be pinning an old snapshot that PASSIVE cannot reclaim
wal_pages=52054 high_water=6000` — Plank 0 detection fired correctly, but `wal_pages` sat at
~8.7x `high_water_pages` with no further mitigation surfaced after the one-shot crossing WARN.
Closing the gap required re-reading this ADR against current `main` (commit `85d30db9`), which
surfaced a codebase change this ADR predates: **ADR-067 (`write-owner-daemon`) introduced
`SqlAccess::atomic_unit`**, a closure-scoped write API where the caller's closure runs inside
the writer task's own transaction and must complete on its first poll (enforced at runtime on
the write-queue path). This is, in substance, the "closure-scoped transaction API" this ADR
named as a future follow-up in Plank 1 above — already delivered, for writes, by a later ADR.
`SqliteTransaction`/`begin_tx` (the API Plank 1's per-statement reject/rollback text was written
against) no longer exists in this codebase; every production write path this ADR's Inventory
(2)/(2b) named (`fold_gate.rs`, `persist.rs`, `sql_bridge.rs`'s writer impls, `curation.rs`,
every store's batch-upsert method, `khive-vcs/sync.rs`) is a synchronous, single-closure-scoped
span bounded to one `spawn_blocking` call — none can be "held across an await" in the sense
Plank 1's stale-op guard was designed to catch.

What shipped for this amendment (`crates/khive-db/src/checkpoint.rs`, `TxAgeSweepState`):
`KHIVE_TX_WARN_SECS`/`KHIVE_TX_MAX_AGE_SECS` are implemented as **config knobs feeding a
background sweep**, not a per-statement guard. On every checkpoint tick — including a tick
where `checkpoint_once` observes `CheckpointTick::Skipped` because the writer mutex is busy,
independent of WAL page pressure either way — the sweep checks
`khive_storage::tx_registry::oldest()`'s age against both thresholds and escalates to
`tracing::warn!`/`tracing::error!` on each below→above crossing (edge-triggered, same debounce
idiom as the WAL-pressure severity ladder — a sustained stale span logs once per rung, not once
per tick). Fix (2026-07-12, same day): the sweep originally ran only on an Observed
tick, which meant a registered `WriterGuard::transaction` span — holding the writer mutex for
its entire registered lifetime — made the checkpoint tick observe `Skipped` and silently
bypassed the sweep for exactly the scenario it exists to catch. The sweep now runs
unconditionally before that early-continue, and additionally tracks the oldest entry's identity
(not just its age) so a stale span that is immediately replaced by an already-stale successor
re-arms and re-emits for the new span rather than staying latched to the departed one. This is
Plank 1's registry-driven half, applied uniformly to every registered span regardless of which
mechanism created it, exactly as originally specified ("applied uniformly to every registered
transaction regardless of which mechanism created it"). It is visibility, not reclamation: no
per-statement rejection or commit-time rollback is implemented, because there is no live call
site left that holds a caller-controlled handle across multiple statements for such a check to
intercept.

This does **not** by itself explain or fix #580's specific 2026-07-12 recurrence. The one
remaining candidate this review turned up at the time that fit "long-lived reader holding a
chunked span open" was `crates/khive-db/src/stores/graph.rs`'s `traverse`, which opened a
deferred read transaction and held it across a `roots.chunks(400)` loop. It was already
registered in `tx_registry` (its own comment named it "the most WAL-pin-relevant span in
the store") and covered by this sweep's _visibility_, but this ADR's original Inventory
item (3) ("a pathologically long single
closure... cannot be fully ruled out for pathological queries") explicitly left any enforcement
for that case as an open question, not a specified mechanism. Bounding or aborting that
traversal past an age cap was a genuine new design decision (which cap, whether a partial-result
error is acceptable to callers, whether other single-closure spans need the same treatment) and
was out of scope for this amendment — tracked as a follow-up rather than invented here, then
resolved by Amendment 4. The other possibility this ADR's own Alternatives section already
named — the pin is outside this process
entirely (a separate `kkernel mcp` stdio session's own connection; `tx_registry` is
process-local and cannot see it) — remains unruled-out and is exactly the "route reads through
the daemon" alternative this ADR already deferred.

### 2026-07-19 amendment (Amendment 2): the pin is cross-process — per-session observability and attribution

**Plank 0 telemetry summary.** A third recurrence
(2026-07-19) provided the discriminating evidence Plank 0 was built to capture.
`wal_pages` sat at 84,000-85,000 (14x `high_water_pages`, 4x
`truncate_high_water_pages`) for at least ten hours. Three TRUNCATE attempts
(22:15, 02:00, 05:58) each made zero progress (`wal_pages_before ==
wal_pages_after`). Across every checkpoint-tick observation in that window, the
in-process registry's oldest open span was **milliseconds to sub-second old**
(`writer_task_tx`, `text_upsert_document` — ordinary bounded writes). The
in-process inventory this ADR audited is therefore exonerated for this
recurrence: no registered span in the daemon held the pin. The pin lives in
another process. Corroborating: a full process-set cycle later that morning (a
binary reinstall killed the daemon; stdio sessions re-exec'd) dropped the WAL
from ~85,000 pages to under 1,000 — the same "killing processes frees the WAL"
signature as #580's original incident.

**Cross-process topology.** At observation time, 13
processes held `khive.db` open directly: the daemon plus 12 `kkernel mcp` stdio
sessions (ages minutes to 10+ hours). Session reads do not route through the
daemon; every session runs its own connection pool against the shared file. The
checkpoint task — and with it the entire Plank 0/Plank 1 sweep — runs **only in
the daemon** (`khive-runtime/src/daemon.rs`, daemon boot path). The processes
most likely to hold the pin are exactly the processes with zero WAL
observability. Channel poll loops are already daemon-gated (#602), so sessions
are pure request-servers; their read/write spans use the same bounded patterns
inventoried above, but nothing observes them, and the `vec0` native cursor
question (Inventory item 4) remains unverified precisely there.

**Decision (additive, observability-first — same posture as Plank 0).**

- **Plank A: per-session registry sweep.** Every `kkernel mcp` process (stdio
  session or daemon) runs the lightweight tx-registry age sweep, not only the
  daemon. For sessions this is observe-only (no PASSIVE/TRUNCATE checkpointing —
  checkpointing stays daemon-owned to avoid N processes competing for the writer
  mutex): a coarse tick (default 5s; sessions do not need the daemon's 500ms
  cadence) checks `tx_registry::oldest()` against the existing
  `KHIVE_TX_WARN_SECS`/`KHIVE_TX_MAX_AGE_SECS` thresholds with the same
  edge-triggered logging.
- **Plank B: cross-process attribution sidecar.** Each process maintains a
  per-PID heartbeat file under `<db-file>.walpin/<pid>.json` containing
  `{pid, process_role, started_at, oldest_tx_age_secs, oldest_tx_label,
  updated_at}`. Written on the sweep tick only when an open span exceeds
  `KHIVE_TX_WARN_SECS` (plus one removal on clean shutdown and on the first tick
  after the condition clears) — quiet processes write nothing, so steady-state
  filesystem traffic is zero. On a TRUNCATE no-progress event, the daemon
  enumerates the sidecar directory and applies a three-test liveness gate (gate
  ruling, 2026-07-19): an entry is live only if (1) its PID is alive, (2) its
  `started_at` matches the OS-reported start time of that PID within a small
  epsilon — a required identity validation, not an advisory cross-check, so a
  reused PID is rejected deterministically rather than probabilistically — and
  (3) its `updated_at` falls within roughly 3 session sweep intervals (the
  sidecar refreshes `updated_at` on every sweep tick while the warn condition
  persists, so a stale timestamp means a crashed process's orphan file).
  Entries failing any test are **deleted** during enumeration, not merely
  skipped, so orphan files cannot accumulate or false-attribute; deletion is
  additionally conditioned on the ownership check below — the daemon removes
  only entries it can attribute to a dead or stale process AND that pass
  ownership validation. The daemon logs every live report alongside its
  existing no-progress WARN. The
  next recurrence therefore names the pinning process directly when a report
  exists. When none does, absence of evidence is attributed only through the
  per-PID sidecar-health distinction below — silence alone never licenses a
  conclusion.

  _Sidecar-health attribution (gate ruling, 2026-07-19)._ A missing heartbeat
  has two very different causes: the process genuinely has no old span, or its
  sidecar never functioned (older binary without the feature, sidecar disabled,
  heartbeat write failed, or the trust-boundary check below refused the
  directory — note that a daemon-side refusal is itself a sidecar-health
  failure and must not masquerade as evidence). To keep the
  zero-steady-state-traffic property while making the two distinguishable,
  each process writes a **registration beacon** at sidecar initialization (a
  per-PID marker whose content is written once and thereafter only
  timestamp-refreshed per the beacon refresh rule below, under the same
  trust-boundary and liveness rules as heartbeats). The census universe is authoritative and
  OS-derived, never sidecar-derived: the set of live database-holding PIDs is
  established by enumerating the processes that hold the database file open at
  the OS level (the same observation that produced the topology count above),
  and sidecar states are then mapped onto that universe. The sidecar directory
  alone cannot define the universe — a database holder that never wrote a
  beacon would be invisible to a sidecar-only census, and the any-unknown rule
  below could never fire for exactly the PIDs it exists to catch. Enumeration
  classifies every PID in the OS-derived census three ways: **reporting**
  (heartbeat present and live), **registered-silent** (live beacon per the
  refresh rule below, no heartbeat — the process affirmatively has no
  over-threshold span), and **unknown** (no beacon, a stale beacon, or a
  database holder absent from all sidecar data — the sidecar's health is
  unestablished; states: disabled, pre-feature binary, write-failed, refused,
  or wedged after initialization). Only a pin observed while every live PID is
  reporting or registered-silent licenses the sharper conclusion that the pin
  is an unregistered/native mechanism (`vec0` cursor, or a span the registry
  does not cover) — the fork needed to justify or reject the deferred
  route-reads-through-the-daemon alternative with evidence. Any `unknown` PID
  makes the attribution inconclusive, and the daemon's WARN names the unknown
  PIDs as the reason.

  _Beacon refresh rule._ Registration at initialization alone never licenses
  `registered-silent`: the beacon proves the sidecar initialized once, not that
  it still functions, and a wedged process whose sweep task has died would
  otherwise hold the pin with exactly the beacon-present/heartbeat-absent
  signature that the sharper conclusion trusts. `registered-silent` therefore
  requires ongoing sidecar liveness: each sweep tick performs a metadata-only
  refresh of the beacon (a timestamp touch of the existing per-PID marker — no
  data write, preserving the zero-steady-state-data-traffic property), and
  classification accepts a beacon only when its refresh timestamp falls within
  the same roughly-3-sweep-interval freshness window and the owning PID passes
  the same identity gate as heartbeats. A stale beacon — and likewise any PID
  whose heartbeat was deleted as stale during enumeration — classifies as
  `unknown`, never `registered-silent`.

  _Sidecar filesystem trust boundary (gate ruling, 2026-07-19)._ The sidecar
  path is predictable, so in a shared or attacker-writable database directory a
  symlinked heartbeat path could otherwise redirect a khive process into
  overwriting an arbitrary file. The write and enumeration contract is
  therefore binding: the `<db-file>.walpin/` directory is created with mode
  `0700` and validated as owned by the current user before any use (refuse the
  directory otherwise — never chmod/chown an existing one into compliance);
  heartbeat writes go through exclusive create with `O_NOFOLLOW` semantics to a
  temporary file followed by atomic rename over the target, never an in-place
  open of a possibly-attacker-placed path; enumeration validates per-entry
  ownership and refuses symlinks before reading or deleting anything.
  Validation binds to an opened handle, not a path: in the attacker-writable
  directory this contract assumes, a path component swapped between a
  path-based validation and the subsequent operation would redirect renames or
  deletions outside the sidecar. The sidecar root is therefore opened once
  with `O_DIRECTORY | O_NOFOLLOW`, its ownership and mode validated on that
  file descriptor, and every subsequent create, rename, unlink, and
  enumeration read performed relative to that descriptor (`openat` /
  `renameat` / `unlinkat` semantics) — the path is never re-resolved per
  operation, and parent components must resolve without traversing a symlink
  at open time.

  _Amendment (clarification of existing intent, 2026-07-20)._ Ancestor path
  components above the database file's containing directory are trusted
  platform layout: an adversary able to substitute or redirect that ancestry
  already controls the database path itself, so sidecar hardening binds
  identity from the database's containing directory downward and makes no
  guarantee against hostile mutation of higher components.
- **Plank C: pin-depth probe via `PRAGMA wal_checkpoint(PASSIVE)` return
  columns.** On a TRUNCATE no-progress event, additionally run
  `PRAGMA wal_checkpoint(PASSIVE)` and report pin depth as `log` minus
  `checkpointed` from its 3-column return row — the number of frames pinned
  behind the backfill boundary. Equivalent signal to reader-mark introspection
  with zero dependence on SQLite's shm WAL-index layout, and PASSIVE never
  blocks readers or writers. The draft's alternative of parsing the shm
  WAL-index directly was struck at the spec gate (2026-07-19) as
  implementation-detail-fragile; do not ship shm parsing.

**Deployment-shape note.** In a single-process deployment the in-process registry already
sees every span, so this amendment adds nothing on that path (the sidecar is a no-op with one
process, and its enumeration output is trivially self-attributing). The multi-process shape
this amendment instruments is any deployment that runs several long-lived processes against
one shared database, so the gap is a product defect class, not a dev-environment quirk.

**Non-goals.** No enforcement changes: thresholds, TRUNCATE policy, and the
visibility-not-reclamation posture are unchanged. No read-routing migration
(Alternative 4) is designed here; Plank B exists to produce the attribution that
decision needs. `vec0` internals remain unverified; Plank B is designed to
implicate or exonerate them without reading native code.

**Config.** `KHIVE_SESSION_SWEEP_INTERVAL_MS` (default 5000, sessions only);
`KHIVE_WALPIN_SIDECAR` (default on for file-backed backends, off for in-memory).
Existing threshold keys are reused unchanged.

### 2026-07-20 amendment (Amendment 3): heartbeat freshness basis and the attribution-basis field

**Motivation.** Two contract refinements to Amendment 2 (#1155 item 1; the
backend-scoped attribution design in
`docs/design/walpin-backend-scoped-attribution.md`):

1. While a transaction is over-threshold, the session sweep rewrites and fsyncs
   the full heartbeat record on every tick (exclusive-create temp file plus
   atomic rename, per the trust-boundary contract). Freshness only needs a
   timestamp to advance, and beacons already refresh with a metadata-only
   mtime touch. The asymmetry is pure write churn: N ticks of one long-running
   span produce N full record writes whose only material delta is the embedded
   age.
2. The backend-scoped attribution design introduces heartbeats written under a
   fallback attribution (an unscoped-origin span observed by the main
   backend's view). The record format needs a field that distinguishes
   evidence-based attribution from fallback so diagnostics never read fallback
   attribution as ground truth. This amendment is the sole source of truth for
   that field's format; the design note consumes the definition and does not
   restate it.

**Plank F1: file mtime is the freshness basis for both record kinds.**

Heartbeat liveness moves to the same basis the beacon refresh rule already
uses: the record's freshness timestamp is the sidecar file's mtime, evaluated
against the unchanged roughly-3-sweep-interval window. While the warn
condition persists, each sweep tick performs a metadata-only mtime touch of
the existing heartbeat file — the same mechanism and the same
opened-directory-descriptor discipline as beacon refresh, preserving the
trust-boundary contract. The full body is rewritten (exclusive-create temp
file plus atomic rename, unchanged) only when record content changes: the
first over-threshold observation, a change of the oldest span's identity or
label, a change of `attribution_basis` (Plank F2), and the removal cases
Amendment 2 already defines (clean shutdown, first tick after the condition
clears). The body's `updated_at` field is retained and now means exactly "the
instant of the last body write"; it no longer participates in liveness
classification.

_Liveness boundary (determinate form)._ Amendment 2 phrased the freshness
window as "roughly 3 session sweep intervals"; on the mtime basis this
amendment replaces that phrasing with an exact, observable contract, because
the window has a hidden indeterminacy: the sweep interval is per-process
configuration (`KHIVE_SESSION_SWEEP_INTERVAL_MS`), and the enumerating daemon
cannot know a remote process's configured value — judging a 60-second-interval
writer by three times the 5-second default would deterministically
misclassify it. Records therefore declare their own cadence: heartbeat and
beacon content each gain a `sweep_interval_ms` field, written at record
creation (a mid-process configuration change is a content change and forces a
rewrite). Classification is then exact:

- **live** iff `enumeration_time - mtime <= 3 x max(declared
  sweep_interval_ms, 1000 ms)` (boundary inclusive);
- otherwise **stale**, with Amendment 2's consequences unchanged (stale
  entries are deleted and their PIDs classify `unknown`).

The `max(..., 1000 ms)` clamp is the mtime-resolution floor: filesystem
mtime granularity is as coarse as one second, so a sub-second declared
cadence must never narrow the classification window below what the basis
can resolve — without the floor, a 100 ms cadence would yield a 300 ms
window and healthy records would classify stale on any coarse filesystem.
Sub-second cadences remain supported on the write side; the clamp affects
classification only, flooring the effective window at three seconds.

Records written by pre-amendment binaries carry no declaration; the
enumerator evaluates them against the default interval (5000 ms), which is
exactly today's behavior. Both timestamps come from the same host — the
sidecar is same-machine by construction, since every writer holds the same
database file — so no cross-host clock skew enters the comparison; filesystem
mtime granularity (worst case one second on coarse filesystems) is covered
by the mtime-resolution floor above. The multiplier's operational meaning is the measurable
spec: a touch is a synchronous same-kernel metadata write, visible to
enumeration the moment the tick completes, so a healthy process's record
never exceeds one declared interval of age plus scheduling jitter, and the
3x window tolerates up to two consecutive missed ticks before classifying
stale. A process that misses more than two ticks is delayed by more than
twice its own declared cadence — which is precisely the wedged-sweep
signature Amendment 2's `unknown` classification exists to surface, so the
false-stale case and the wanted-detection case coincide at the boundary.

_Age is computed at read time._ A body that is not rewritten cannot carry a
current age. The heartbeat record gains `oldest_tx_started_at` (epoch
timestamp of the oldest span's registration instant); enumeration computes
the current age as now minus `oldest_tx_started_at`. The existing
`oldest_tx_age_secs` field is retained with its documentation narrowed: the
age as of the last body write. Readers prefer `oldest_tx_started_at` when
present; records written by pre-amendment binaries lack it and are read
exactly as before. The alternative — keeping age current by rewriting the
body every tick — is the status quo this plank exists to remove.

_Recovery rule._ The touch path must never assume the file still exists:
enumeration deletes stale entries, and a slow writer's heartbeat can be
deleted while its span is still live. On every sweep tick where the warn
condition holds, a missing heartbeat (or a touch failing with not-found) is
recreated by a full body write through the unchanged create path — so a
deletion costs at most one tick of attribution gap, never permanent
invisibility. Beacons recover the same way, fail-closed: a missing beacon is
rewritten on the next tick, and a beacon that cannot be recreated is a
sidecar-health failure — the process classifies `unknown`, per Amendment 2's
rule that a daemon-side or write-side failure must never masquerade as
affirmative evidence. The full record lifecycle is therefore closed, with
every transition defined: **create** (first over-threshold observation, full
body write) → **touch** (each subsequent tick while the condition persists,
metadata-only) → **rewrite** (content change, full body write) → **stale**
(mtime falls outside the declared window) → **delete** (by enumeration, or
by the writer on clean shutdown / first tick after the condition clears) →
**recreate** (next over-threshold tick, identical to create). No state is
terminal while the underlying span is live.

_Mixed-version rule._ An enumerator predating this amendment classifies by
body `updated_at` and would delete a live heartbeat whose new-style writer
touches only mtime — the old reader actively destroys live records, which is
the dangerous direction. The normative rule is therefore
**readers-before-writers**: after a binary upgrade, the enumerating daemon
process is restarted onto the amended binary before new-style writers
matter. This is the deployment's natural order — the daemon is restarted at
reinstall while stdio sessions re-exec afterward — so the ordering is the
default behavior, not an operational burden; readers upgraded to this
amendment accept both generations (records carrying the new fields classify
on mtime; records without them classify on `updated_at` exactly as before),
so upgraded readers never misjudge old writers. Should the ordering ever be
violated (an old daemon process still running against new-style writers),
the contract bounds the damage rather than pretending the window away: the
window exists only between binary upgrade and daemon restart, an old-reader
deletion is repaired by the recovery rule on the writer's next tick, and the
interim classification is `unknown` — inconclusive, never false attribution.
No flag-day coordination is required.

_Crash conservatism._ An mtime touch is not durability-critical: a touch lost
to a crash makes the record look stale, and Amendment 2's liveness gate
already deletes stale entries and classifies their PIDs as `unknown` — the
failure direction is inconclusive attribution, never false attribution. This
is the same conservatism the beacon refresh rule accepted.

**Plank F2: the `attribution_basis` field.**

The heartbeat record gains exactly one additive field:

- `attribution_basis`: string with exactly two values.
  - `"origin"` — the oldest span carried this database's own origin
    identity.
  - `"fallback"` — the oldest span was unscoped and is observed by the main
    backend's view as the designed never-silently-drop fallback.
- Absence: records written by binaries predating this field carry no
  `attribution_basis`; readers treat absence as unspecified — neither origin
  nor fallback may be inferred from a missing field.

_Fail-closed reading rule (binding on every consumer)._ Classification of a
record's attribution confidence fails closed: a missing `attribution_basis`,
an unrecognized value, or any parse failure of the field classifies the
record as unspecified/fallback-confidence — **never** as evidence-backed.
Only the exact string `"origin"` licenses the evidence-backed reading. This
rule is versioned with the field itself: mixed-version readers encountering
records from newer writers with values this amendment does not define must
degrade to fallback-confidence rather than guess. Without this rule a
fallback attribution could be read as ground truth — the exact confusion the
field exists to prevent.

The origin semantics (which spans are scoped to which database, and why
unscoped spans fall back to the main view) are specified by the
backend-scoped attribution design note; this amendment owns only the field's
name, type, values, and the reading rule above. A change of
`attribution_basis` is a content change and forces a body rewrite under
Plank F1. Beacons carry no attribution and are unchanged beyond the cadence
declaration.

**Non-goals.** Thresholds, the enumeration liveness gate structure, census
rules, the filesystem trust boundary, and the beacon refresh mechanism are
unchanged; beacon content changes only by gaining the same `sweep_interval_ms`
declaration heartbeats gain. This amendment adds no fields beyond the three
named here (`oldest_tx_started_at`, `attribution_basis`, `sweep_interval_ms`).

### 2026-08-01 amendment (Amendment 4): bounded traversal work and statement snapshots

**Motivation.** Issues #1443 and #1444 close the unresolved pathological-query
case in inventory item (3). The SQLite graph store previously evaluated a
recursive CTE to exhaustion, retained every emitted path row in Rust, and only
then applied the caller's `limit`. It also wrapped every root chunk in one
deferred read transaction. A small response limit therefore bounded neither
SQL work nor retained rows, while a dense traversal could pin one WAL snapshot
for the full caller-controlled operation.

**Public shape bounds.** One `traverse` operation has the following fixed
ceilings. Validation happens before storage work, and a violation is an invalid
input error rather than silent clamping:

- at most **100 raw roots**, checked before name/ID resolution; resolved UUIDs
  are then de-duplicated while preserving first-root order;
- `max_depth` defaults to 3 and may not exceed **10**, retaining the bound
  already established by ADR-008 and ADR-012;
- `limit` counts non-root first visits independently per distinct root,
  defaults to **100**, and may not exceed **1,000**; a depth-0 root never
  consumes this quota.

**Execution bounds.** Every public operation owns one non-serializable
execution budget shared by all request clones used to read visible namespaces:

- at most **100,000 returned adjacency rows** may be admitted across all roots
  and namespaces; rows count before visited-set de-duplication, so self-loops,
  parallel paths, and already-seen nodes consume work;
- a **five-second** wall-clock deadline covers storage traversal, enforced at
  frontier/row boundaries and by SQLite's VM progress handler so a statement
  that stops producing rows is still interruptible.

To distinguish "exactly exhausted" from "more work exists," storage may read
one additional, non-retained sentinel row. That row is reported in
`usage.graph_hops` because it was actually returned by SQLite, but it is not
admitted by the 100,000-row budget and causes the whole operation to fail. A
work or deadline failure returns **no partial path set**.

**Traversal algorithm.** SQLite traversal is level-synchronous breadth-first
search driven by indexed, direction-specific adjacency statements. A node is
marked visited on first enqueue, which guarantees its retained occurrence has
minimum depth. Storage stops reading immediately when the root's result quota
is full. Same-depth edge/node order remains backend-defined and is not a public
ordering contract; `Direction::Both` may process its two indexed arms in a
fixed implementation order.

**Snapshot lifetime.** The traversal no longer opens a deferred transaction
around the operation. Each adjacency statement runs in autocommit mode and
owns one `graph_traverse_read` registry span, so its WAL snapshot ends when
that statement/cursor is dropped. Concurrent commits may consequently become
visible between frontier expansions. This is the chosen consistency contract:
the runtime already reads visible namespaces independently and never promised
one cross-namespace point-in-time snapshot, while bounded statement snapshots
remove the caller-controlled WAL pin. Attribution remains backend-scoped under
Amendment 2 even though each span is now intentionally short-lived.

**Consequences.** Dense and cyclic graph tests must assert shallowest-depth
results plus measured work at low limits, boundary tests must cover every
public ceiling and the over-budget sentinel, and snapshot tests must prove a
live statement is attributed while no registry entry survives the statement.
Changing any numeric ceiling, partial-result rule, traversal ordering guarantee,
or snapshot consistency model requires another ADR amendment.

### 2026-08-02 amendment (Amendment 5): dedicated checkpoint connection — supersedes `try_writer_nowait`

**Motivation.** Production evidence (2026-08-02): 22 caller-side `pool.writer()`
admission timeouts across the fleet, sustained bursts, against a persistent
64MiB WAL. `checkpoint_once` acquired the pool's writer mutex via
`try_writer_nowait` (Plank 2, above) and held that same guard across `PRAGMA
wal_checkpoint(PASSIVE)` — and, when armed, the TRUNCATE escalation — for the
whole tick. Over a large WAL a PASSIVE pass can run long enough that every
concurrent `pool.writer()` caller times out at `checkout_timeout`. This was an
unforced application-level constraint, not a SQLite requirement: `PRAGMA
wal_checkpoint` takes SQLite's CKPT lock, not the WRITE lock, so a writer can
commit concurrently with a passive checkpoint. Serializing checkpoint and
writers through one mutex imposed contention SQLite itself never required.

**This amendment supersedes, and directly contradicts, several statements
made earlier in this document** (Plank 2's own section, above, and the
Non-goals section's "does not redesign writer serialization" — that statement
was true of the pool's general-purpose write path, which this amendment does
not touch, but was never meant to bless serializing the checkpoint task's own
pragmas behind that same mutex). Both are corrected by this amendment; the
earlier text is left in place, unedited, as the historical record of the
original (now-superseded) design, per this document's own convention for
prior amendments.

**Design.** The checkpoint task now owns a dedicated, long-lived standalone
connection to the same database file (`CheckpointConnection` in
`checkpoint.rs`), opened once at task startup via the crate's existing
standalone-connection open path (`ConnectionPool::open_standalone_writer` —
same pragmas as any other standalone connection, including `busy_timeout`
from the pool config). `checkpoint_once` runs PASSIVE — and, when armed,
`maybe_truncate`'s TRUNCATE escalation, on the SAME dedicated connection,
never a second checkout — on this connection and never checks out the pool's
writer mutex at all. `try_writer_nowait` is no longer used anywhere on this
path.

**Skip semantics changed.** Previously a busy pool writer caused
`checkpoint_once` to return `CheckpointTick::Skipped` for that tick (a busy
writer skip). That mechanism is gone: PASSIVE now runs unconditionally on
every tick regardless of concurrent write traffic, because it no longer
contends with that traffic at all. `CheckpointTick::Skipped` still exists, but
is now produced only when the dedicated connection itself is unavailable —
never opened yet (an in-memory or read-only pool has no on-disk file, or no
write permission, for `open_standalone_writer` to open a second connection
against), or dropped after a prior tick's connection-level pragma failure.
`CheckpointConnection::ensure_open` lazily reopens on the next tick in that
case; a dropped connection never crashes the task.

**What actually changed is admission, not SQLite-level blocking — this
distinction matters and must not be flattened.** The dedicated connection
removes the pool-mutex ADMISSION path: a `pool.writer()` checkout no longer
queues behind a checkpoint tick, because that tick no longer holds the pool's
writer mutex at all. SQLite-level lock semantics are otherwise unchanged by
this amendment: PASSIVE takes only the CKPT lock and never blocks writers, at
the SQLite level, on any connection. TRUNCATE inherits RESTART semantics and
_additionally_ acquires SQLite's writer lock, blocking new write transactions
on ALL connections — this process or any other, pool-checked-out or
standalone — while it waits on pinning readers, bounded by
`truncate_busy_timeout` (see
[`sqlite3_wal_checkpoint_v2`](https://www.sqlite.org/c3ref/wal_checkpoint_v2.html)).
So during an armed TRUNCATE, a concurrent caller is still ADMITTED promptly —
that is what the reproducer in
`crates/khive-db/tests/checkpoint_dedicated_connection.rs` asserts — but a
write transaction it then attempts can still wait, at the SQLite level, for
the bounded TRUNCATE window, exactly as before this amendment. "Never falls on
a concurrent `pool.writer()` caller" is true of admission only; it must not be
read, here or anywhere else in this document or the accompanying source, as a
claim that TRUNCATE's SQLite-level write-blocking window disappeared.

**What did not change.** Every counter, WARN-once threshold-crossing
semantic, the tx-registry age sweep (Plank 1, including its running on a
Skipped tick — the reason changed, from "writer busy" to "dedicated
connection unavailable," but the invariant that the sweep must not go blind
during a Skipped streak did not), the severity ladder (Plank 0), and the
walpin sidecar heartbeat (Amendment 2 Plank B) are behavior-neutral under
this amendment — only which connection/lock a checkpoint tick runs on
changed. `run_checkpoint_task`'s public signature, and every other daemon
call site, are also unchanged.

### 2026-08-08 amendment (Amendment 6): independent bounded sidecar collection

**Motivation.** Amendment 2 coupled `walpin::enumerate_live` — the operation that removes
dead, reused-PID, stale, and malformed trusted entries — to the TRUNCATE-no-progress diagnostic
arm. A healthy database never enters that arm, so crash residue accumulated even though the
enumerator already had a 512-entry work bound. The same coupling also tempted diagnostics to
serialize cleanup counters as `0`/`false` when no enumeration occurred, making "not measured"
look like a measured clean state (#1794, #1795).

**Decision.** Every sidecar-enabled daemon checkpoint tick performs at most one bounded sidecar
pass for that task's file-backed backend. When the tick does not enter TRUNCATE-no-progress
attribution, the task refreshes its own beacon/heartbeat and runs the housekeeping-specific
`walpin::housekeep_live` pass before the Observed/Skipped branch. This pass uses the session-sweep
legacy-record cadence fallback captured once at daemon-task startup
(the ADR-defined compiled default, 5000 ms), never either the daemon's 500 ms checkpoint cadence
or the enumerating process's current environment override. It removes only entries whose producer
is positively dead or whose PID start identity proves reuse. A live PID's malformed,
uninspectable, or stale heartbeat/beacon remains on disk and classifies `unknown`; housekeeping
must not consume the evidence that a later no-progress report needs.

When a TRUNCATE attempt makes no progress, the existing `walpin::enumerate_live` attribution pass
runs instead. The synchronous PASSIVE/TRUNCATE core records an immutable attribution request; it
never enumerates the directory itself. Before the async checkpoint task may consider ordinary
housekeeping or emit that tick's lifecycle outcome, it consumes the request in an awaited
`tokio::task::spawn_blocking` and only then uses the classifications for the operator report. The
pass may remove trusted malformed/stale residue under Amendment 2's original cleanup rule. The
checkpoint state is marked attempted before the blocking worker starts, so both success and an
indeterminate worker failure suppress ordinary housekeeping later in the same tick: a panicked
worker may already have performed part of the walk, and a second scan would violate the bound.
Thus the 512-entry cap applies to one sidecar-directory pass per tick rather than silently allowing
a second 512-entry scan. Collection remains independent of WAL size, threshold crossings, and
checkpoint availability on every tick that did not already run attribution. An operator who
explicitly disables the sidecar also disables collection. A trust-boundary enumeration error or
blocking-worker join failure is surfaced distinctly, warned, and never flattened into successful
attribution or allowed to terminate the checkpoint loop.

`db_diagnostics` remains non-destructive with respect to sidecar evidence: the request does not
invoke `enumerate_live`. Its cleanup-derived `sidecar_listing_truncated` and
`sidecar_entries_cleanup_would_reap` members are optional and omitted when enumeration did not
run. A background checkpoint tick may independently perform its normal cleanup, but the
diagnostic request itself never converts an unmeasured value into a clean-looking zero.

### 2026-08-09 amendment (Amendment 7): admitted cached-reader snapshots

**Correction.** The original inventory predates the file-backed `SqlBridge` connection cache.
`SqlAccess::reader()` and queue-backed `SqlWriter` reads now retain a read-only connection across
calls; connection lifetime alone is not a snapshot lifetime, but an unfinalized statement or
unadmitted transaction on that cache would reproduce the multi-hour WAL pin this ADR governs.
Issue #1828 also showed that charging an idle cached connection against `max_readers` exhausts the
process-local admission budget even when the connection is correctly in autocommit.

**Decision.** An idle cached reader owns no reader permit and must be in autocommit. Each ordinary
query acquires a permit for its blocking SQLite operation and releases it only after the statement
is finalized and autocommit is verified. One explicit top-level deferred read transaction is
allowed as one logical read operation: its successful `BEGIN` transfers the operation permit onto
the handle and installs a backend-scoped `tx_registry` span; queries reuse both guards;
`COMMIT`/`END` or full `ROLLBACK` releases them only after SQLite reports autocommit.
Immediate/exclusive starts and nested transaction controls remain rejected. Cancellation or handle
drop destroys the connection before its retained transaction permit and registry handle, so there
is never an idle WAL snapshot outside admission or invisible to the checkpoint age sweep. See
ADR-005's 2026-08-09 amendment for the full raw-SQL capability contract.

**Checkpoint acceptance.** The integration regression
`multiple_long_lived_idle_cached_readers_allow_bounded_checkpoint_progress` retains eight idle
cached reader handles against a two-reader budget after each handle completes its one-shot read,
while repeated write cycles run with SQLite autocheckpoint disabled. The dedicated Amendment 5
checkpoint connection copies every WAL frame on each PASSIVE cycle and the WAL file remains
bounded. This is #1828's permit-lifetime acceptance: idle handles no longer retain reader
admission, without depending on a zero-reader TRUNCATE window or reintroducing per-writer
autocheckpoint (#1848).

This regression does not reproduce or close #1460 or #1812. An idle autocommit connection does
not pin WAL; those issues concern production stdio/multiprocess pinning and continuous
concurrent-session WAL bounds, respectively, and remain open pending their own
production-shaped regressions and fixes.

### 2026-08-09 amendment (Amendment 8): routine logical WAL and writer-stage telemetry

**Motivation.** Physical `-wal` bytes are an allocation high-water mark, not a logical backlog:
SQLite can reset and reuse a large sidecar after every frame has been backfilled. Conversely, the
old periodic path discarded most of the PASSIVE result and operational writer telemetry flattened
queue admission, write-lock acquisition, application work, and COMMIT/fsync into one duration.
Those shapes could not distinguish retained allocation from a pinned checkpoint boundary, or
queue contention from a slow transaction body (#1849).

**Routine WAL decision.** A normal checkpoint tick issues exactly one
`PRAGMA wal_checkpoint(PASSIVE)` and retains its complete `(busy, log, checkpointed)` row. The
former no-argument observation followed by an explicit PASSIVE second pass is removed. Thresholds
continue to use `log`; `pending = max(log - checkpointed, 0)` is exposed independently. The same
tick records physical sidecar bytes and an observation timestamp. Samples are keyed by canonical
backend identity because checkpoint tasks fan out in multi-backend deployments. The daemon's
metrics-only frame reads the sample for its own main pool and exposes `wal_log_frames`,
`wal_checkpointed_frames`, `wal_pending_frames`, `wal_physical_bytes`, and sample time; `wal_pages`
remains a compatibility alias for logical log frames. A scrape is a pure memory read and causes no
additional checkpoint I/O or filesystem stat. The explicit `db_diagnostics` probe remains an
on-demand, checkpoint-performing diagnostic with its existing contract.

**Writer-stage decision.** Each writer-task request timestamps (1) construction before bounded
channel admission through dequeue (`queue_wait`), (2) only `BEGIN IMMEDIATE`
(`transaction_acquire`), (3) only the typed operation closure (`body`), and (4) only `COMMIT`.
Backend-keyed latest-stage gauges and the existing slow-write row expose those microsecond fields,
the total, queue depth, and observation time. Telemetry is published before the typed reply. A
top-level request or a phase that never ran reports zero for the inapplicable phase; rollback and
recovery time is not mislabeled as COMMIT and remains visible as total minus the named stages.
This is observation only: no timing value changes admission, retry, rollback, checkpoint, or
TRUNCATE behavior.

### 2026-08-09 amendment (Amendment 9): checkpoint telemetry cannot amplify a pinned WAL

**Motivation.** A production WAL pin exposed a feedback loop in the ADR-094 lifecycle sink.
`run_checkpoint_task` appended `CheckpointOutcomeRecorded` to the primary store on every
at/above-`warn_pages` observation. A pinned reader prevented those event writes from being
reclaimed, so the 500 ms checkpoint loop became a high-rate writer to the WAL it was trying to
drain. The event handoff was already lossy under sink contention; paying one primary-store write
per attempt therefore provided neither complete history nor storage safety (#1838).

**Decision.** Checkpoint pressure persistence is edge-triggered. One elevation row is enqueued
when pressure first reaches `warn_pages`, sustained elevated observations aggregate in bounded
task memory, and one recovery row is enqueued after pressure returns below `warn_pages`.
`CheckpointOutcomeRecordedPayload` carries `episode_elevated_ticks` and
`episode_peak_wal_pages`; the recovery row is the complete episode summary, while a delayed
opening enqueue reports the aggregate observed so far. Both fields are absent on legacy rows
written before this amendment; new transition rows use `payload_schema_version = 2`. A full
bounded handoff leaves the accepted elevation state unchanged, so that transition may be retried
without admitting one store append per checkpoint attempt. A recovery enqueue is likewise
retried on later healthy ticks until accepted. If no elevation row ever reached the handoff, no
orphan recovery row is invented.

The invariant is now: with A checkpoint attempts inside one uninterrupted pressure episode,
primary-store lifecycle appends are O(state transitions) (normally two), never O(A). The
checkpoint loop's per-attempt evidence remains in the existing tracing/debug path and in honest
process-global diagnostics:

- `checkpoint_pressure_elevated_ticks`;
- `checkpoint_pressure_episodes_started` / `checkpoint_pressure_episodes_recovered`;
- `checkpoint_lifecycle_append_attempts` / `checkpoint_lifecycle_append_failures`;
- `checkpoint_lifecycle_enqueue_drops`.

These are lifetime aggregates across every checkpoint task in the process, matching the scope of
the pre-existing ADR-091 counters. They are not reset by the operator surface. The actual
pressure ladder remains `CheckpointSeverityState`'s in-memory consecutive-observation machine;
it does not query persisted per-tick events. WAL-pin sidecars, no-progress attribution, PASSIVE /
TRUNCATE policy, and the one-sidecar-pass-per-tick bound are unchanged.

**Failure direction.** Lifecycle persistence remains best-effort. A queue drop or append failure
can leave a gap in durable transitions, but it increments an explicit diagnostic counter and
never creates a primary-store retry loop. The checkpoint task's essential operator evidence is
the transition row, recovery summary when deliverable, process counters, edge-triggered logs,
and WAL-pin attribution—not a self-amplifying per-attempt event stream.

### 2026-08-09 amendment (Amendment 10): disable per-connection WAL autocheckpoint

**Motivation.** Amendment 5 moved scheduled checkpoint work to one dedicated connection, but
SQLite's automatic checkpoint threshold is connection-local. A non-zero threshold on any
ordinary writer still runs an implicit PASSIVE checkpoint synchronously in the commit that
crosses it. Disabling the threshold only on the dedicated connection therefore cannot keep
checkpoint I/O off application commit paths.

**Decision.** Checkpoint ownership is claimed, not assumed. The scheduled checkpoint task claims
ownership of its pool at startup (`ConnectionPool::claim_checkpoint_ownership`, plus a
propagation call that reaches a writer task spawned before the claim). On a claimed pool every
writer-capable connection sets `PRAGMA wal_autocheckpoint = 0`: the already-open pooled writer
is re-configured under the writer mutex, and the single standalone-writer constructor applies
the claimed value to store and SQL-bridge writers as well as the writer task, diagnostics
connection, and dedicated checkpoint connection, including every later open or checkpoint
reconnect. On a pool no checkpoint task claims — embedded runtimes and one-shot CLI executions
have writable pools but never start the scheduled task — writer-capable connections keep a
bounded 4,000-page autocheckpoint, so SQLite's own reclamation still bounds WAL growth where no
dedicated owner exists. The former `PoolConfig::wal_autocheckpoint_pages` field and
`KHIVE_WAL_AUTOCHECKPOINT_PAGES` override are removed: routine checkpoint ownership is a safety
invariant, not a tuning parameter, and neither posture is selectable by configuration.

The scheduled task remains the only in-process source of routine PASSIVE checkpoint I/O on
claimed pools. Amendment 5's dedicated-connection admission contract, TRUNCATE gates and busy
bound, sidecar collection, counters, and severity ladder are otherwise unchanged. Regressions
read the pragma on each connection class in both postures and grow a WAL past the 4,000-page
threshold before observing it, so configuration-only coverage cannot mask a later-created
connection reverting to the wrong posture — including the unclaimed-pool regression that the
bounded fallback exists to prevent: unbounded WAL growth on a writable pool with no checkpoint
owner.

### 2026-08-09 amendment (Amendment 11): write-transaction external-work audit

This amendment was allocated as Amendment 11 at integration; Amendments 9 and 10 record the
checkpoint-telemetry and autocheckpoint changes merged ahead of it.

**Motivation.** WAL mode admits one writer. Queue time therefore depends on both mean writer hold
time and its variance; a filesystem call, network request, blocking wait, or expensive compute
step inside `BEGIN IMMEDIATE` extends every competing writer's wait while remaining invisible to
queue-side admission telemetry. The earlier inventory proved transaction lifetimes were scoped,
but did not enumerate what actually ran inside each write scope. The audit for #1850 found one
real violation: `FsBlobStore::transactional_orphan_sweep` performed file metadata checks and
unbounded file deletion inside `SqlAccess::atomic_unit`.

**Normative invariant.** From `BEGIN IMMEDIATE` until COMMIT/ROLLBACK, application code may execute
SQLite statements plus bounded in-memory binding/result bookkeeping only. It MUST NOT perform
filesystem/process/network I/O, sleep or block on a non-SQL synchronization primitive, call another
subsystem, or perform model/embedding/unbounded computation. SQLite's own database/WAL/VFS work is
of course part of statement execution and is not “external work” in this rule.

**Complete production write-scope audit (current tree).** The owner row is the review unit; every
production caller named in that row was inspected through its commit/rollback edge.

| Transaction owner                               | Production scopes/callers                                                                                      | Work inside the transaction                                                   | Verdict                                 |
| ----------------------------------------------- | -------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------- | --------------------------------------- |
| `run_migrations_locked` and `apply_schema_plan` | Core versioned migrations; pack service migrations                                                             | Migration DDL/DML and ledger insert                                           | SQL-only                                |
| `WriterGuard::transaction`                      | Pack auxiliary DDL; runtime symmetric edge update; entity/note merge fallback                                  | Synchronous statement sequences over one borrowed connection                  | SQL-only                                |
| `writer_task::drain_loop`                       | All `send`/`send_bounded` store mutations, queue-backed `SqlBridge` batches, and `atomic_unit` requests        | The request's prepared SQL statements and bounded row/result folding          | SQL-only after the blob-GC repair below |
| `SqlBridge` manual owners                       | Standalone and pool-backed `execute_batch`; flag-off `run_manual_atomic_unit`                                  | Pre-prepared parameterized statements, commit/rollback, poisoning bookkeeping | SQL-only                                |
| Store flag-off batch owners                     | `entity`, `note`, `event`, `graph`, `text`, `sparse`, `vectors`, and `agents` batch/upsert/delete methods      | Bounded per-item SQL loops and result counters                                | SQL-only                                |
| Vector-store private IMMEDIATE transactions     | Vector batch upsert/delete/orphan reconciliation                                                               | sqlite-vec/ordinary table statements and bounded row binding                  | SQL-only                                |
| Retrieval weight private IMMEDIATE transaction  | `engine_weights::apply_weight_delta_with_eta`                                                                  | One scalar read, bounded EMA arithmetic, weight upsert, and audit-row insert  | SQL-only                                |
| Runtime/pack `AtomicUnitOp` callers             | Runtime atomic runner and ANN registry; brain fold/persist; session mirror ingest; blob recovery/claim/cleanup | DML/query statements and bounded validation/folding                           | SQL-only                                |
| Blob physical GC (outside owner)                | `FsBlobStore::transactional_orphan_sweep`                                                                      | Root walk, metadata, advisory locking, and file deletion                      | Explicitly outside SQLite transactions  |

**Blob cross-resource repair.** The sweep now prepares its file candidates before SQLite opens a
writer transaction. The protocol first holds a process-local lock keyed by the canonical database
path and a cross-process `<database>.khive-blob-gc.lock`, then takes the existing root locks. This
database-scoped ownership serializes differently configured roots as well as identical roots. Once
acquired, every pre-existing claim in that database is abandoned; after fail-closed validation it
is removed in transactions of at most 128 rows. Recovery therefore does not depend on the mutable
path-derived `root_key` and also covers a relocated root or an online-backup snapshot restored at a
different database path.

Candidate processing is likewise split into units of at most 128. Each short atomic unit anti-joins
live `entities.content_ref` values and commits only that bounded set of durable `blob_gc_claims`;
V20 entity INSERT/UPDATE triggers reject a new live reference to any claimed digest. After commit,
the sweep deletes only that batch's files outside SQLite and removes only that batch's claims in a
second bounded atomic unit before advancing. JSON bindings, claim-table mutations, returned rows,
and application result folding are therefore cardinality-bounded per writer hold. A crash between
units leaves the trigger fence durable and fail-closed; the next exclusive database owner rescans
the filesystem and liveness evidence before recovering it. This keeps the stronger ADR-111
liveness guarantee without retaining SQLite's single writer across external I/O or creating one
orphan-population-sized claim transaction.

`transactional_orphan_sweep_releases_sqlite_writer_before_physical_delete` pauses at the exact
claim/physical boundary, proves an unrelated writer commits while deletion is parked, and proves a
racing claimed reference is rejected. `transactional_orphan_sweep_bounds_each_durable_claim_batch`
pins the 128-row active-claim peak,
`abandoned_claim_recovery_deletes_at_most_one_batch_per_writer_hold` pins bounded recovery, and
`transactional_orphan_sweep_recovers_claims_after_root_relocation` plus
`transactional_orphan_sweep_recovers_claims_copied_by_database_restore` pin path-independent
abandoned-claim recovery across both relocation and backup restore.
`cancelling_sweep_during_delete_keeps_owner_locks_until_blocking_work_finishes` proves cancellation
cannot release either advisory owner while already-started blocking deletion continues.

**Review guard.** This table is normative and exhaustive. Any PR that adds or widens a
`BEGIN IMMEDIATE`, `TransactionBehavior::Immediate`, `WriterGuard::transaction`, writer-task
request body, or `SqlAccess::atomic_unit` caller MUST update the applicable row (or add one) and
show that all inputs/external results are prepared before the transaction opens. The
`AtomicUnitOp` trait documentation repeats this requirement because first-poll enforcement catches
async suspension but cannot detect synchronous filesystem calls. A new site whose table entry is
absent or whose body violates the invariant is a defect.

The hold-time regression parks physical deletion indefinitely after the claim commit and gives an
unrelated writer a 100 ms SQLite busy bound. The unrelated commit succeeds inside that bound while
deletion is still parked; under the former one-transaction implementation it remained behind the
parked filesystem phase and reached the busy timeout. This is the measured boundary for #1850:
external deletion contributes zero time to SQLite's exclusive writer hold.

### 2026-08-11 amendment (Amendment 12): abandoned reads release WAL snapshots

Request abandonment is now an active snapshot-lifetime boundary. MCP
`notifications/cancelled`, daemon peer EOF/reset, daemon shutdown, coordinator
search timeout, and the general request-read deadline merge into one absolute
read context. A SQLite read installs the common progress/interrupt guard on
its exact connection; graph traversal contributes its own budget predicate to
that same callback. `knowledge.compose` also checks the context between awaits,
inside domain/atom loops, and during synchronous tokenization/BM25/scoring so a
caught degradable backend error cannot start later reads after cancellation.

Stdio EOF cancels the exact rmcp root token before rmcp starts its graceful
drain, so each in-flight request child reaches this read context immediately
instead of running for the drain's five-second allowance. The canonical DSL
dispatch boundary installs the default request-read deadline for wire calls,
daemon frames, local/operator execution, and provenance-verified scheduled
replay. A previously installed outer deadline remains authoritative when it is
earlier; the boundary never renews it. Detached warm, checkpoint, sweep, and
channel-maintenance tasks do not inherit a request context unless they are an
explicit response-owned child wrapped by the context-inheritance helper.

An interrupted explicit reader transaction finalizes its cursor and rolls
back before callback removal and permit release. If rollback or handler
cleanup cannot prove a clean autocommit connection, that connection is closed
rather than cached. Consequently an abandoned read cannot keep a WAL tail
pinned after its interrupt settles; the regression establishes a real table
snapshot, observes `wal_checkpoint(PASSIVE)` pinned before cancellation, then
requires `log == checkpointed`, prompt sole-reader recovery, stopped VM
progress, and no callback bleed on reuse.

"Settles" is a two-stage bound, not a single grace window. `spawn_blocking`
cannot be force-aborted once started, so the async boundary cannot prove a
worker's connection and admission were actually released without joining the
real worker. On grace expiry it does not detach: it escalates to a second,
longer join bounded by `KHIVE_SQLITE_INTERRUPT_HARD_CAP_MS`. Ordinary
interrupted work — including a slow UDF or table-valued call that SQLite
cannot check the interrupt flag inside of — settles within this window, and
the caller's typed timeout is only returned once the real worker has joined,
so admission and any WAL snapshot are provably released by the time the
caller sees the response. Only a worker that ignores the interrupt for the
entire grace-plus-hard-cap window (a callback/UDF that never returns) is
detached; that worker's connection and admission are not recovered until it
eventually exits on its own, and detachment is logged at `error` with the
operation name so it is distinguishable from ordinary settlement. This is
strictly narrower than "keeps a WAL tail pinned after its interrupt settles":
it is "pinned only while genuinely hostile work has not yet settled, bounded
by grace plus the hard cap." A cancellation arriving while a raw-SQL
statement is still being prepared and classified — before it is known to be
a read or a write — takes the same bounded path rather than the
completion-preserving one: nothing has executed against SQLite yet, so
abandoning the wait cannot strand a write mid-flight. Only a statement that
has actually been classified as an admitted write/transaction-control
statement and started executing is completion-preserving.

Operator controls are `KHIVE_REQUEST_READ_TIMEOUT_SECS` (default 30, valid
1–3600 seconds, invalid/zero falls back to the nonzero default),
`KHIVE_SQLITE_INTERRUPT_GRACE_MS` (default 500, valid 10–5000 ms), and
`KHIVE_SQLITE_INTERRUPT_HARD_CAP_MS` (default 5000, valid 100–60000 ms, the
second-stage join bound above). These bound read work and interrupt
settlement only. They do not change write admission, commit, rollback,
checkpoint, or TRUNCATE policy.
