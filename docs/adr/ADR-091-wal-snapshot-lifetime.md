# ADR-091: Bounded read-transaction lifetime and WAL checkpoint escalation

**Status**: Proposed
**Date**: 2026-07-04
**Depends on**: ADR-015 (schema migrations), ADR-049 (daemon warm state)
**Fixes**: [#580](https://github.com/ohdearquant/khive/issues/580)

## Context

Live incident, 2026-07-04 (#580): `~/.khive/khive.db` was 3.7GB; `khive.db-wal` had grown
to 15.5GB (15,512,941,272 bytes); `-shm` was 30MB. The fleet was running roughly three
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

### Round-1 review correction (this section replaces the original draft's Plank 1 basis)

Codex round-1 review of this ADR rejected the original mechanism on two Blockers, both
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
risk, but not demonstrated as the incident's cause.** `sql_bridge.rs:848-894` opens a
**standalone** connection and issues an explicit `BEGIN` (`BEGIN DEFERRED` for read-only,
`BEGIN IMMEDIATE` for read-write, `BEGIN EXCLUSIVE` for serializable;
`sql_bridge.rs:869-882`) that stays open, on that one connection, for exactly as long as
the caller holds the returned `SqliteTransaction` before calling `commit()` or letting it
drop. This is the one place in the codebase where transaction duration is fully
caller-controlled rather than bounded by a single synchronous closure. However:
tracing every production call site of `begin_tx` (`grep -rn "begin_tx(" crates`) finds
exactly two non-test callers, `khive-pack-session/src/mirror/ingest.rs:615` and `:2416`,
both `SqlTxOptions::default()` (`read_only: false`, `SqlIsolation` not `Serializable`),
which resolves to `BEGIN IMMEDIATE`, a **write** transaction, not the read-only
`BEGIN DEFERRED` path. Both call sites are bounded batch loops (one mirror-ingest pass
over a file's new events) that commit at the end of the function; neither is held across
a poll-loop sleep (`mirror/service.rs` sleeps at `service.rs:348` with no open
transaction or connection carried across that await; every tick reopens what it needs).
The read-only `BEGIN DEFERRED` branch requires either an explicit
`SqlTxOptions { read_only: true, .. }` caller (none exists in the tree today) or the
entire backend opened via `StorageBackend::sqlite_read_only` (`backend.rs:46-70`, an
opt-in config path via `cfg.read_only` in `serve.rs:1209`, not the default `khive.db`
backend construction). **This mechanism is real and worth bounding defensively, but it
is a latent risk under today's call graph, not a proven explanation for #580.**

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
strongest remaining candidates, in order of plausibility, are: (2) if a future or
missed caller ever holds a `begin_tx` transaction across a long idle span (not currently
demonstrated), (4) `vec0`'s internal behavior (unverified, native code, needs targeted
instrumentation or upstream documentation review), and (3) a pathologically long bounded
query (self-terminating, doesn't match the ">24h idle process" shape well). Per
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
Plank 1 bounds the one mechanism proven to allow caller-controlled read-transaction
duration (`begin_tx`), plus the in-memory/test pooled-reader path the original draft
targeted, narrowed to the surface it actually covers. Plank 2 (TRUNCATE escalation)
carries over from the original draft largely unchanged, with an explicit flap/backoff
statement added per Leo's request.

### Plank 0: instrumentation before enforcement tuning

Because Plank 1's thresholds cannot be responsibly chosen without knowing which
mechanism is real, add observability first and treat it as a prerequisite deliverable,
not an optional nice-to-have:

- On every `run_checkpoint_task` tick (`checkpoint.rs:141-183`), in addition to the
  existing `wal_pages` observation, log (`tracing::debug!` normally, escalating to
  `tracing::warn!` once `wal_pages` crosses `warn_pages`, matching the existing
  rate-limited crossing pattern) the age of the oldest currently-open `begin_tx`
  transaction, if any (see Plank 1's tracking below), and the current WAL frame count.
- On a TRUNCATE attempt (Plank 2) that fails to make progress (`wal_pages_after` within a
  small epsilon of `wal_pages_before`), enumerate and log every currently-open `begin_tx`
  transaction's start time, elapsed duration, and (if the caller supplied one) a label,
  extending `SqlTxOptions` with an optional `label: Option<&'static str>` mirroring the
  `SqlStatement::label` convention already used elsewhere (`ingest.rs`'s
  `label: Some("session_mirror_insert_message")` pattern). This directly answers the
  question this ADR could not answer from static reading: which specific caller, if any,
  is holding the pin, the next time this happens in production.
- This data gates Plank 1's threshold tuning: `KHIVE_TX_MAX_AGE_SECS` (below) ships with
  a conservative default and is explicitly called out as provisional pending one cycle
  of production telemetry from this plank.

### Plank 1: bound the one caller-controllable read-transaction path, retarget the rest

**`begin_tx` transaction age bound (new, replaces the original Plank 1's primary
mechanism).** `SqliteTransaction` (`sql_bridge.rs:401-407`) gains an `opened_at: Instant`
field, set when `begin_tx` issues its `BEGIN` (`sql_bridge.rs:882-883`). Two enforcement
points, both because a transaction on a standalone connection cannot be safely
force-rolled-back from a different thread than the one holding it:

- **Soft cap (logging only):** every `execute`/`query_row`/`query_all` call on the
  transaction (`sql_bridge.rs:411-538`) checks `opened_at.elapsed()` and logs a
  rate-limited `tracing::warn!` (same edge-triggered pattern as `crossing_warn`,
  `checkpoint.rs:224-228`) once it exceeds `KHIVE_TX_WARN_SECS` (default **30s**;
  provisional, see Plank 0). Includes the transaction's `label` if the caller supplied
  one.
- **Hard cap (fail-closed):** once `opened_at.elapsed()` exceeds
  `KHIVE_TX_MAX_AGE_SECS` (default **120s**; provisional, see Plank 0), subsequent
  `execute`/`query_row`/`query_all` calls on that transaction return an error instead of
  running the statement, forcing the caller's own error-handling path to abort and drop
  the transaction. `SqliteTransaction` has no `Drop` impl today (`sql_bridge.rs:401-407`);
  dropping the struct drops its `Option<Connection>`, and `rusqlite::Connection::drop`
  closes the connection, which SQLite auto-rolls-back on close. This is already correct
  behavior; the hard cap makes sure a caller that ignores the soft-cap WARN and keeps
  looping cannot hold the transaction open indefinitely. No committed work is lost that
  the caller didn't already fail to commit; this is strictly a bound on worst-case
  duration, not a change to commit semantics.
- `KHIVE_TX_WARN_SECS` / `KHIVE_TX_MAX_AGE_SECS` are deliberately generous relative to
  the two known production callers (bounded per-file mirror-ingest batches, expected to
  complete in well under a second per file in normal operation) so this cap is a safety
  net for a runaway loop or a future caller, not a routine limit.

**Pooled `ReaderGuard` recycling: keep, narrow the claim.** The original draft's
age/op-count recycling on `return_reader` (`pool.rs:434-454`) is retained exactly as
designed, because it is harmless and still correct hygiene, but the ADR no longer claims
it protects production file-backed traffic: it only ever executes for in-memory/test
`ConnectionPool` instances (see the Round-1 correction above). State this explicitly so a
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
- **Explicit flap/backoff behavior (Leo's addition):** if `try_writer_nowait()` itself
  fails (the writer mutex is held by a concurrent write) at the moment a TRUNCATE attempt
  is due, the attempt is skipped for that tick exactly like an ordinary PASSIVE skip; the
  task does not retry within the same tick or spin-wait. `last_truncate_attempt` is
  **not** updated on a skip (only on an attempt that actually acquired the writer), so
  the next tick where the writer is free is eligible immediately rather than waiting out
  the full `truncate_min_interval` again. If the writer is busy for a sustained period
  (multiple consecutive ticks), each tick's ordinary PASSIVE attempt still runs
  (`try_writer_nowait` failing for TRUNCATE doesn't preclude a separate successful
  PASSIVE elsewhere in the same tick loop, since both share the one `try_writer_nowait`
  call per tick, and a busy writer skips both simultaneously per the current tick
  design). **Accepted worst case, stated explicitly:** if the writer is continuously busy
  for the entire observation window, TRUNCATE never runs and the WAL keeps growing past
  `truncate_high_water_pages`; the mitigation is the existing rate-limited WARN
  escalating in severity at higher multiples of the threshold (e.g. WARN at 1x, a second
  distinct WARN at 4x) so sustained failure to reclaim is visible to an operator rather
  than silently absorbed, rather than promising unconditional reclamation, which would
  require blocking writer acquisition (rejected, see original Alternatives).
- Observability: unchanged from the original draft (`tracing::info!` per attempt with
  before/after page counts and elapsed time; `tracing::warn!` after three consecutive
  attempts fail to clear `warn_pages`), extended per Plank 0 to also log open `begin_tx`
  transactions when an attempt fails to make progress.

### Config summary

| Key                                    | Default | Plank | Purpose                                                                                      | Status                                     |
| -------------------------------------- | ------- | ----- | -------------------------------------------------------------------------------------------- | ------------------------------------------ |
| `KHIVE_TX_WARN_SECS`                   | 30      | 1     | Soft-cap WARN on an open `begin_tx` transaction's age                                        | New, provisional pending Plank 0 telemetry |
| `KHIVE_TX_MAX_AGE_SECS`                | 120     | 1     | Hard-cap: reject further statements on a transaction past this age                           | New, provisional pending Plank 0 telemetry |
| `KHIVE_READER_MAX_AGE_SECS`            | 300     | 1     | Recycle a pooled reader connection past this age on return (in-memory/test pool only)        | Carried over, scope narrowed               |
| `KHIVE_READER_MAX_OPS`                 | 5000    | 1     | Recycle a pooled reader connection past this op count on return (in-memory/test pool only)   | Carried over, scope narrowed               |
| `KHIVE_READER_CHECKOUT_WARN_SECS`      | 10      | 1     | WARN when the oldest outstanding pooled checkout exceeds this age (in-memory/test pool only) | Carried over, scope narrowed               |
| `KHIVE_WAL_TRUNCATE_HIGH_WATER_PAGES`  | 20000   | 2     | WAL page count that arms a TRUNCATE attempt                                                  | Carried over                               |
| `KHIVE_WAL_TRUNCATE_MIN_INTERVAL_SECS` | 300     | 2     | Minimum spacing between successful TRUNCATE attempts                                         | Carried over                               |
| `KHIVE_WAL_TRUNCATE_BUSY_MS`           | 2000    | 2     | Temporary busy_timeout override during a TRUNCATE attempt                                    | Carried over                               |

Existing, unchanged: `KHIVE_CHECKPOINT_INTERVAL_MS` (500), `KHIVE_WAL_WARN_PAGES` (2000),
`KHIVE_WAL_HIGH_WATER_PAGES` (6000), `KHIVE_JOURNAL_SIZE_LIMIT_BYTES` (64MiB),
`KHIVE_BUSY_TIMEOUT_SECS` (30), `KHIVE_CHECKOUT_TIMEOUT_SECS` (5).

## Alternatives considered

1. **WAL2** (upstream SQLite's two-rotating-WAL-file mode). Rejected: not shipped in the
   stable `rusqlite`/bundled `libsqlite3` version khive depends on; adopting it would
   mean vendoring a patched SQLite build for a config-and-scheduling-level fix. Revisit
   only if WAL2 reaches upstream stable and the config-level fix proves insufficient.
2. **External checkpointer process** (litestream-style out-of-process WAL manager).
   Rejected: khive embeds SQLite in-process by design (single 7.7MB binary); an external
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
   #580 unless the actual pin turns out to be a `begin_tx` write path that a
   daemon-mediated design would also need to serialize correctly.

## Failure modes

- **`begin_tx` hard-cap rejection during a legitimate long-running batch.** If a future
  caller legitimately needs a transaction open longer than `KHIVE_TX_MAX_AGE_SECS`
  (120s default), the hard cap forces it to fail and retry in smaller batches. This is
  an intentional trade: no code path in the tree today needs a transaction anywhere near
  that long (mirror-ingest batches are per-file and complete well under a second in
  normal operation); if this becomes a real constraint, raise the cap rather than remove
  it, using Plank 0's telemetry to confirm the caller's real needs first.
- **TRUNCATE contention**: bounded to `truncate_busy_timeout` (default 2s) per attempt,
  at most once per `truncate_min_interval` under normal conditions (see the flap/backoff
  note: a skipped attempt due to writer contention does not consume the interval).
- **Flap under sustained writer load**: per the explicit backoff statement above, if the
  writer is continuously busy, TRUNCATE never fires and WAL growth continues past
  `truncate_high_water_pages`; accepted, escalating WARNs are the mitigation, not
  unconditional reclamation.
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
- WAL growth now has one new, real, caller-controllable bound: no `begin_tx` transaction
  can hold a connection open past `KHIVE_TX_MAX_AGE_SECS`, closing the one mechanism this
  review confirmed is both live and fully under a single caller's control.
- Plank 0's instrumentation is the load-bearing deliverable of this ADR's first
  iteration: it converts "we don't know what's pinning the WAL" into a concrete,
  loggable answer the next time sustained WAL pressure occurs, which Plank 1's
  provisional thresholds and any follow-up ADR amendment can then be tuned against.
- The existing periodic PASSIVE checkpoint tick, its skip-on-busy behavior, and its
  `warn_pages`/`high_water_pages` WARN semantics are unchanged; TRUNCATE escalation is
  additive to `checkpoint.rs`, not a rewrite, with an explicit accepted-worst-case
  statement for sustained writer contention.
- Two new config knobs for the `begin_tx` bound (Plank 1), three carried-over knobs
  narrowed in scope, three for TRUNCATE escalation (Plank 2); the two new keys are
  explicitly marked provisional pending one cycle of production telemetry rather than
  presented as tuned defaults.
- Follow-up (tracked separately, not blocking this ADR): once Plank 0 telemetry
  identifies whether `vec0`'s internal cursor behavior, a missed `begin_tx` caller, or
  something else entirely is the actual #580 mechanism, file a short ADR amendment
  narrowing or retuning Plank 1 rather than re-guessing from static code reading again.
