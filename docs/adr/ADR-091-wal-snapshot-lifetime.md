# ADR-091: Bounded read-transaction lifetime and WAL checkpoint escalation

**Status**: Accepted (ratified 2026-07-05)
**Date**: 2026-07-04
**Depends on**: ADR-015 (schema migrations), ADR-049 (daemon warm state)
**Fixes**: [#580](https://github.com/ohdearquant/khive/issues/580)

## Context

Live incident, 2026-07-04 (#580): `~/.khive/khive.db` was 3.7GB; `khive.db-wal` had grown
to 15.5GB (15,512,941,272 bytes); `-shm` was 30MB. The deployment was running roughly three
concurrent implementer agents plus a warm daemon. Writes started failing with
`sqlite: invalid data: timed out after 5s waiting for sqlite writer connection` on
`comm.send` and other write ops. `PRAGMA wal_checkpoint(PASSIVE)` returned `0|3768965|44`:
the writer was not busy, 3,768,965 frames sat in the WAL, and only 44 were checkpointable.
Process census at the time: one `kkernel mcp --daemon` plus seven `kkernel mcp [--db ...]`
stdio sessions, several live parents more than 24h old. Killing the idle stdio processes
freed the WAL, which is the load-bearing datum this ADR's mechanism has to explain: some
per-process, long-lived state was pinning the checkpoint boundary, and closing the
process (not merely being idle) released it.

`0|3768965|44` means SQLite's own checkpoint boundary, the oldest live reader's mark, had
barely moved across 3.77M frames. `PRAGMA wal_checkpoint(PASSIVE)` never blocks and never
reclaims past the oldest reader; it is by design incapable of doing more than this once a
reader pins the tail. The write timeouts are a downstream symptom: as the WAL and `-shm`
grow, `wal-index` operations degrade and both readers and the writer do more work per
statement, but the presenting error (`timed out ... waiting for writer connection`) is a
`khive-db` writer-mutex checkout timeout (`crates/khive-db/src/pool.rs:308-319`), not a
SQLite-level lock. `PASSIVE` reported `busy=0`, so the writer mutex itself was not
contended at diagnosis time; the writer was simply slow underneath a bloated WAL, or hit
timeouts during separate bursts. Root cause is squarely "something pinned the checkpoint
boundary," not writer contention.

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
  drain-failure counter across N=3 consecutive checkpoint cycles at `warn_pages`, tracked
  as khive#617 and not yet implemented) and, once `truncate_high_water_pages` is crossed,
  the shipped ALARM/TRUNCATE-escalation tier in this plank, rather than promising
  unconditional reclamation, which would require blocking writer acquisition (rejected,
  see original Alternatives).
- Observability: unchanged from the original draft (`tracing::info!` per attempt with
  before/after page counts and elapsed time; `tracing::warn!` after three consecutive
  attempts fail to clear `warn_pages`), extended per Plank 0 to also log every open entry
  in the shared transaction registry (both `begin_tx` and raw `SqlWriter` transactions)
  when an attempt fails to make progress.

### 2026-07-04 amendment: severity ladder + `wal_pages` units

**Severity ladder (this corrects Plank 0's crossing-severity wording above).** Plank 0's
description of the `warn_pages` crossing (`escalating to tracing::warn! once wal_pages
crosses warn_pages`, matching the currently-shipped `crossing_warn` gate at
`checkpoint.rs:277-294`) is superseded: crossing `warn_pages` (default 2000,
`KHIVE_WAL_WARN_PAGES`) on its own is **INFO**, not WARN, because it is an expected,
self-resolving event under ordinary write bursts, not an operator-actionable condition.
The ladder is:

- **INFO**: `wal_pages` crosses `warn_pages` (a single tick observation).
- **WARN**: `wal_pages` fails to drain back below `warn_pages` across **N = 3** consecutive
  checkpoint cycles (each cycle is one `run_checkpoint_task` tick, default 500ms via
  `KHIVE_CHECKPOINT_INTERVAL_MS`). N is owned by maintainers and tunable. **This tier is
  not yet implemented.** It is distinct from the shipped `note_truncate_outcome` escalation
  (`checkpoint.rs:508-530`), which counts consecutive TRUNCATE _attempts_, not checkpoint
  cycles, and only ever runs once `wal_pages` has already crossed the much higher
  `truncate_high_water_pages` (default 20000) and `maybe_truncate` (`checkpoint.rs:428-506`)
  has actually attempted a TRUNCATE, gated by `truncate_min_interval` (default 5 minutes).
  Pressure that sits at, say, 3000 pages indefinitely (above `warn_pages` but far below
  `truncate_high_water_pages`) never reaches `maybe_truncate` at all and so never fires
  `note_truncate_outcome`. Building the ruling's WARN tier (a drain-failure counter keyed to
  `warn_pages` and ordinary checkpoint ticks, not TRUNCATE attempts) is tracked as khive#617.
- **ALARM**: the Plank 2 TRUNCATE-escalation tier, armed by `truncate_high_water_pages`
  (default 20000, `KHIVE_WAL_TRUNCATE_HIGH_WATER_PAGES`, "a separate, much higher threshold
  than `high_water_pages`", `checkpoint.rs:109-119`) via `maybe_truncate`
  (`checkpoint.rs:428-506`). Crossing `high_water_pages` (default 6000,
  `KHIVE_WAL_HIGH_WATER_PAGES`, the crossing-WARN block at `checkpoint.rs:296-304`) remains
  a shipped intermediate log between the WARN and ALARM tiers, but it is not itself a ladder
  tier: it neither arms nor performs any TRUNCATE attempt, and must not be conflated with the
  `truncate_high_water_pages` crossing that actually does.

Downgrading the shipped `warn_pages`-crossing log call (`checkpoint.rs:289`, currently
`tracing::warn!`) to `tracing::info!`, and building the N=3 drain-failure WARN tier
described above, are both tracked as khive#617; neither is implemented by this ADR's
current code.

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
   Long-lived stdio sessions are live Claude Code seats; killing them by policy is a
   worse user experience than bounding transaction/connection lifetime underneath them.
   Also notable: the incident's own workaround (killing idle processes freed the WAL)
   is exactly this alternative applied manually, which is precisely why it is not an
   acceptable long-term policy rather than evidence the mechanism is understood.
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
  most of what the deferred closure-scoped-API follow-up would have targeted; the
  remaining un-bounded spans are `graph.rs`'s chunked-traversal read snapshot
  (`graph_traverse_read`) and any future caller of a registry-registered span this ADR
  did not anticipate.
- **TRUNCATE contention**: bounded to `truncate_busy_timeout` (default 2s) per attempt,
  at most once per `truncate_min_interval` under normal conditions (see the flap/backoff
  note: a skipped attempt due to writer contention does not consume the interval).
- **Flap under sustained writer load**: per the explicit backoff statement above, if the
  writer is continuously busy, TRUNCATE never fires and WAL growth continues past
  `truncate_high_water_pages`; visibility via the severity ladder (see the 2026-07-04
  amendment: the WARN drain-failure tier, khive#617, and the shipped ALARM/TRUNCATE tier)
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
  is INFO, WARN (not yet implemented, khive#617) is reserved for a 3-consecutive-cycle
  drain failure, and `truncate_high_water_pages` arming the TRUNCATE escalation is the
  ALARM tier. `high_water_pages` crossing remains a shipped intermediate log, not a ladder
  tier on its own.
- Two new config knobs for the shared transaction-registry sweep (Plank 1), covering
  every `khive_storage::tx_registry`-registered span — `begin_tx`'s historical
  `SqliteTransaction` target no longer exists; the real coverage today is `atomic_unit`'s
  registered span for every production write path, plus `graph.rs`'s chunked-traversal
  read snapshot — three carried-over knobs narrowed in scope, three for TRUNCATE
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
remaining candidate this review turned up that fits "long-lived reader holding a chunked span
open" is `crates/khive-db/src/stores/graph.rs`'s `traverse`, which opens a deferred read
transaction and holds it across a `roots.chunks(400)` loop — already registered in `tx_registry`
(its own comment names it "the most WAL-pin-relevant span in the store") and now covered by this
sweep's _visibility_, but this ADR's original Inventory item (3) ("a pathologically long single
closure... cannot be fully ruled out for pathological queries") explicitly left any enforcement
for that case as an open question, not a specified mechanism. Bounding or aborting that
traversal past an age cap is a genuine new design decision (which cap, whether a partial-result
error is acceptable to callers, whether other single-closure spans need the same treatment) and
is out of scope for this amendment — tracked as a follow-up rather than invented here. The other
possibility this ADR's own Alternatives section already named — the pin is outside this process
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
- **Plank C: pin-depth probe via `PRAGMA wal_checkpoint(PASSIVE)` return
  columns.** On a TRUNCATE no-progress event, additionally run
  `PRAGMA wal_checkpoint(PASSIVE)` and report pin depth as `log` minus
  `checkpointed` from its 3-column return row — the number of frames pinned
  behind the backfill boundary. Equivalent signal to reader-mark introspection
  with zero dependence on SQLite's shm WAL-index layout, and PASSIVE never
  blocks readers or writers. The draft's alternative of parsing the shm
  WAL-index directly was struck at the spec gate (2026-07-19) as
  implementation-detail-fragile; do not ship shm parsing.

**Deployment-shape note.** The hosted khive-cloud topology is single-process:
the in-process registry already sees every span there, and this amendment adds
nothing to that path (the sidecar is a no-op with one process, and its
enumeration output is trivially self-attributing). The multi-process shape this
amendment instruments is local multi-seat operation — which is also the
many-agents-one-substrate deployment khivedb ships as, so the gap is a product
defect class, not a dev-environment quirk.

**Non-goals.** No enforcement changes: thresholds, TRUNCATE policy, and the
visibility-not-reclamation posture are unchanged. No read-routing migration
(Alternative 4) is designed here; Plank B exists to produce the attribution that
decision needs. `vec0` internals remain unverified; Plank B is designed to
implicate or exonerate them without reading native code.

**Config.** `KHIVE_SESSION_SWEEP_INTERVAL_MS` (default 5000, sessions only);
`KHIVE_WALPIN_SIDECAR` (default on for file-backed backends, off for in-memory).
Existing threshold keys are reused unchanged.
