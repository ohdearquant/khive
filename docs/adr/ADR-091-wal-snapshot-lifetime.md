# ADR-091: Bounded read-snapshot lifetime and WAL checkpoint escalation

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
stdio sessions, several live parents more than 24h old.

`0|3768965|44` means SQLite's own checkpoint boundary, the oldest live reader's snapshot
mark, had barely moved in 3.77M frames. `PRAGMA wal_checkpoint(PASSIVE)` never blocks and
never reclaims past the oldest reader; it is by design incapable of doing more than this
once a reader pins the tail. The write timeouts are a downstream symptom: as the WAL and
`-shm` grow, `wal-index` operations degrade and both readers and the writer do more work
per statement, but the presenting error (`timed out ... waiting for writer connection`)
is a `khive-db` writer-mutex checkout timeout (`crates/khive-db/src/pool.rs:308-319`),
not a SQLite-level lock. The writer mutex itself was uncontended; the writer was simply
slow underneath a bloated WAL.

### What the codebase already does (as of `origin/main`, verified 2026-07-04)

- `crates/khive-db/src/pool.rs` implements one writer (`Mutex<Connection>`) plus an
  `ArrayQueue` of `max_readers` pooled reader connections (default:
  `min(available_parallelism, 8)`, `pool.rs:15,64-67`). `ReaderGuard::drop`
  (`pool.rs:145-156`) returns a pooled connection via `return_reader` (`pool.rs:434-454`).
- `return_reader` already defends against one failure mode: an **explicit** transaction
  left open on a reader (`reset_reader_connection`, `pool.rs:561-582`) issues `ROLLBACK`
  and re-opens the connection if that fails or the connection fails a liveness probe
  (`reader_connection_is_healthy`, `pool.rs:584-597`). This does **not** address an
  **implicit** (autocommit) read snapshot: `conn.is_autocommit()` is `true` for a bare
  `SELECT` the instant the last row is read, so `reset_reader_connection` short-circuits
  to `true` (`pool.rs:562-564`) without doing anything, even though the connection's WAL
  read mark can, in practice, still reference the point-in-time snapshot from whichever
  statement it last executed until that connection runs a new statement. A reader
  connection idle in the pool queue between checkouts carries its last snapshot forward
  indefinitely; with 8 readers and uneven per-session query volume, some pooled
  connections can sit unused for long stretches while still anchored at an old WAL
  position.
- `crates/khive-db/src/checkpoint.rs` (module added since the last WAL incident) already
  runs a periodic background task (`run_checkpoint_task`, `checkpoint.rs:141-183`),
  spawned once per daemon (`crates/khive-runtime/src/daemon.rs:498-502`). It issues
  `PRAGMA wal_checkpoint(PASSIVE)` every `interval` (default 500ms,
  `checkpoint.rs:53-58`) via `try_writer_nowait` (`pool.rs:337-344`, zero-wait try-lock,
  skips the tick rather than stalling behind write traffic) and logs a rate-limited WARN
  once WAL pages cross `warn_pages` (default 2000, ~8MB) or `high_water_pages` (default
  6000, ~24MB; `checkpoint.rs:60-75`).
- The module's own doc comment (`checkpoint.rs:12-21`) explains **why TRUNCATE is
  deliberately excluded** from this periodic task: `TRUNCATE` inherits `RESTART`
  semantics, it invokes the busy handler and waits (up to the pool's `busy_timeout`,
  default 30s, `pool.rs:29-32,69-74`) for open reader snapshots to release before it can
  reset the WAL file. Running that on a 500ms periodic tick could stall all writes for
  up to 30s. So today, past `high_water_pages`, the daemon only WARNs; nothing in the
  codebase currently reclaims the WAL automatically once PASSIVE stops making progress.
  This is exactly the gap the incident hit: `high_water_pages` (6000) was crossed by
  three orders of magnitude (3,768,965 observed) with no escalation path.
- `journal_size_limit_bytes` (`pool.rs:44-49`, default 64MiB, `KHIVE_JOURNAL_SIZE_LIMIT_BYTES`)
  is already configured on the writer connection. This pragma bounds the WAL file size
  that a **successful TRUNCATE checkpoint** will shrink down to; it does not, by itself,
  cause a checkpoint to run. PASSIVE checkpoints reclaim frames logically (advance the
  boundary SQLite reuses) but do not shrink the file on disk. Only TRUNCATE (or, on
  some platforms, the `wal_autocheckpoint`-triggered auto-checkpoint that itself may run
  in TRUNCATE-adjacent modes under certain conditions) resizes the `-wal` file. The
  15.5GB number in the incident is a **file size**; closing the gap between "frames
  checkpointed" and "bytes reclaimed on disk" requires TRUNCATE to actually run.
- Read paths in the hot query surface (`crates/khive-runtime/src/retrieval.rs:384-460`,
  `hybrid_search`) check out and release a reader per storage call (`self.text(token)?
  .search(...).await`, `self.vector_search(...)`), not held across the whole request.
  The per-call-site discipline is already followed in the code we inspected. No call
  site under `crates/khive-db/src/stores/*.rs` opens an explicit read-side `BEGIN`
  (grep confirmed: every `BEGIN IMMEDIATE` in the tree is on a writer path,
  `stores/note.rs:433`, `stores/entity.rs:325`, `stores/vectors.rs:356`,
  `stores/text.rs:298,363,1111`, `stores/graph.rs:352`, `stores/sparse.rs:249`,
  `stores/event.rs:707,722`). This rules out "explicit read `BEGIN` never committed" as
  the mechanism and points instead at (a) the idle-pooled-connection-carries-forward-a-
  stale-implicit-snapshot mechanism described above, and/or (b) a handler holding a
  `ReaderGuard` across a slow non-DB step (a long embedding call, a large unbounded
  result iteration) rather than releasing it before that step. Both are real risks under
  the incident's topology: seven independent `kkernel mcp` processes each running their
  own `ConnectionPool` against the same file, several sessions live for >24h.

### Non-goals

This ADR does not redesign writer serialization (the single-writer-mutex model is
unchanged), does not change journal mode away from WAL, and does not add general
connection-pool observability beyond what is needed to diagnose and bound this specific
defect class. Batch-write contention and multi-writer scaling are tracked separately.

## Decision

Two complementary planks. Plank 1 bounds how long any single reader connection can pin
the WAL tail. Plank 2 gives the daemon a safe, bounded way to reclaim WAL bytes once
PASSIVE checkpointing stalls behind a pinned reader, including one that briefly outlives
Plank 1's bound.

### Plank 1: bounded read-snapshot lifetime

**Primary mechanism: recycle pooled reader connections by age, unconditionally, on
return.**

Extend `return_reader` (`crates/khive-db/src/pool.rs:434-454`) so that every pooled
reader connection is closed and reopened once it has been alive longer than a configured
age, regardless of `is_autocommit()` state. This is a deliberate blunt instrument: rather
than trying to detect whether a connection still references a stale WAL snapshot (which
`is_autocommit()` cannot answer, see Context), closing and reopening the physical
connection unconditionally guarantees the OS/SQLite-level grip on the old WAL position is
released, because a fresh `Connection::open_with_flags` call takes a fresh snapshot only
when it next reads.

- New `PoolConfig` field: `reader_max_age: Duration`, default **300s (5 min)**.
  Overridable via `KHIVE_READER_MAX_AGE_SECS`. Tracked by recording an `opened_at:
  Instant` alongside each pooled `Connection` (the pool currently stores bare
  `Connection` values in the `ArrayQueue<Connection>`, `pool.rs:106`; this becomes
  `ArrayQueue<PooledReader>` where `PooledReader { conn: Connection, opened_at: Instant }`,
  an additive, internal-only change with no public API break).
- New `PoolConfig` field: `reader_max_ops: u64`, default **5000**. A secondary bound for
  connections that stay busy enough to never go idle long enough to cross the age bound;
  incremented per checkout, checked alongside age in `return_reader`.
- `return_reader` logic becomes: if `reset_reader_connection` fails, or the connection
  fails `reader_connection_is_healthy`, or `opened_at.elapsed() >= reader_max_age`, or
  `ops_count >= reader_max_ops`, close (`close_connection_quietly`, `pool.rs:599-604`)
  and replace with a freshly opened connection (`open_reader_connection`,
  `pool.rs:369-376`) before pushing back onto the queue. This is strictly additive to
  the existing explicit-transaction check; it never removes it.
- Recycling only ever happens inside `return_reader`, i.e. when the connection is _not_
  checked out. A guard's `Drop` always completes its return before the connection is
  eligible for recycling, so a live borrower is never yanked out from under it (see
  Failure modes).

**Diagnostic complement: checkout-age watchdog.** Age-based recycling on return cannot
help a reader that is checked out and held for a very long time (a handler bug, or a
slow non-DB step performed while still holding the guard); recycling only fires when
the guard comes back. Add lightweight outstanding-checkout tracking to `ConnectionPool`:
an `AtomicU64` counter of the oldest currently-outstanding checkout's start time (a
single global watermark is sufficient; per-lease tracking is not needed for a warning
signal). Expose `ConnectionPool::oldest_checkout_age(&self) -> Option<Duration>`. The
existing periodic task (`run_checkpoint_task`, `checkpoint.rs:141-183`) samples this once
per tick and logs a rate-limited WARN (reusing the `crossing_warn` pattern already in
that file, `checkpoint.rs:224-228`) when it exceeds `KHIVE_READER_CHECKOUT_WARN_SECS`
(default **10s**). This closes today's blind spot: when a WAL high-water WARN fires,
there is currently no signal correlating it to a specific checkout being open for Ns;
this makes that correlation visible in the same log stream.

### Plank 2: daemon-side TRUNCATE escalation

The periodic task keeps its existing behavior unchanged for every ordinary tick: PASSIVE
only, `try_writer_nowait`, skip-on-busy (`checkpoint.rs:196-214`). This plank adds a
second, much rarer escalation path rather than modifying that hot loop, so the
already-tested non-blocking guarantee for normal ticks is untouched.

- New `CheckpointConfig` fields:
  - `truncate_high_water_pages: u64`, default **20,000 pages** (~80MB at 4KiB pages).
    Deliberately well above `high_water_pages` (6000, WARN-only) so TRUNCATE is reserved
    for sustained pressure, not transient bursts. Overridable via
    `KHIVE_WAL_TRUNCATE_HIGH_WATER_PAGES`.
  - `truncate_min_interval: Duration`, default **5 minutes**. Minimum spacing between
    TRUNCATE attempts, so a stuck reader cannot cause repeated blocking attempts in a
    tight loop. Overridable via `KHIVE_WAL_TRUNCATE_MIN_INTERVAL_SECS`.
  - `truncate_busy_timeout: Duration`, default **2000ms**. A temporary busy-timeout
    override applied to the writer connection immediately before the TRUNCATE attempt
    and restored to the pool's configured `busy_timeout` (default 30s) immediately
    after, win or lose. Overridable via `KHIVE_WAL_TRUNCATE_BUSY_MS`. This bounds the
    worst-case write stall from a TRUNCATE attempt to about 2s instead of inheriting the
    full 30s `busy_timeout`, matching the reasoning already documented for why the
    periodic PASSIVE path avoids TRUNCATE (`checkpoint.rs:12-21`). The fix here is not
    "run TRUNCATE at the same cadence with a short timeout" (still resource-costly if
    attempted every 500ms), it is "run TRUNCATE occasionally, only under sustained
    pressure, with its blocking window capped."
- `run_checkpoint_task` logic addition, evaluated only on `CheckpointTick::Observed`
  ticks (a skipped tick already `continue`s and is unaffected): if `wal_pages >=
  truncate_high_water_pages` and `last_truncate_attempt.elapsed() >=
  truncate_min_interval`, acquire the writer via the same `try_writer_nowait` already
  held for the PASSIVE call in that tick (never a separate, blocking writer checkout),
  temporarily set `busy_timeout = truncate_busy_timeout` via
  `PRAGMA busy_timeout=<ms>`, issue `PRAGMA wal_checkpoint(TRUNCATE)`, restore
  `busy_timeout` to the pool's configured value, and record `last_truncate_attempt =
  now()` regardless of outcome (so a failed or timed-out attempt still respects the
  min-interval before retrying).
- Observability: `tracing::info!` on every TRUNCATE attempt with `wal_pages_before`,
  `wal_pages_after` (re-query via `query_wal_pages`, `checkpoint.rs:242-246`), and
  `elapsed`. `tracing::warn!` when three consecutive TRUNCATE attempts each fail to bring
  `wal_pages` back under `warn_pages`. This is the signal that recycling (Plank 1) is
  not resolving the pin, most likely a genuinely leaked (never-returned) reader guard
  rather than an idle stale-snapshot connection, and should page an operator.
- Interaction with Plank 1: Plank 1 bounds how long any pooled reader can pin the tail
  (`reader_max_age`, default 300s) and surfaces long-held checkouts
  (`oldest_checkout_age`). Plank 2's TRUNCATE attempts happen at 5-minute-minimum
  spacing, so in steady state a TRUNCATE attempt is likely to occur after at least one
  full reader-recycle cycle has already rolled off the previous generation of pinned
  snapshots. The two planks compound rather than duplicate effort.

### Config summary

| Key                                    | Default | Plank | Purpose                                                               |
| -------------------------------------- | ------- | ----- | --------------------------------------------------------------------- |
| `KHIVE_READER_MAX_AGE_SECS`            | 300     | 1     | Force-recycle a pooled reader connection past this age on return      |
| `KHIVE_READER_MAX_OPS`                 | 5000    | 1     | Force-recycle a pooled reader connection past this op count on return |
| `KHIVE_READER_CHECKOUT_WARN_SECS`      | 10      | 1     | WARN when the oldest outstanding checkout exceeds this age            |
| `KHIVE_WAL_TRUNCATE_HIGH_WATER_PAGES`  | 20000   | 2     | WAL page count that arms a TRUNCATE attempt                           |
| `KHIVE_WAL_TRUNCATE_MIN_INTERVAL_SECS` | 300     | 2     | Minimum spacing between TRUNCATE attempts                             |
| `KHIVE_WAL_TRUNCATE_BUSY_MS`           | 2000    | 2     | Temporary busy_timeout override during a TRUNCATE attempt             |

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
   worse user experience than bounding their reader-connection lifetime underneath them.
4. **Route all reads through the daemon instead of per-process pools** (collapse "N
   independent `ConnectionPool`s against one file" into one daemon-mediated reader path).
   This would remove the multi-pool topology that made the incident's seven independent
   pools possible in the first place, and is a natural extension of ADR-049's daemon
   warm-state model. Noted as a future direction, out of scope here: it requires an MCP
   transport change (stdio sessions proxying reads through the daemon socket) beyond a
   bounded connection-lifetime fix.

## Failure modes

- **TRUNCATE contention**: bounded to `truncate_busy_timeout` (default 2s) per attempt,
  at most once per `truncate_min_interval` (default 5 min). Worst case: one write-path
  caller stalls up to 2s once every 5 minutes under sustained WAL pressure, an explicit,
  bounded trade against unbounded WAL growth.
- **Reader starvation / yanked connection**: recycling in Plank 1 only acts inside
  `return_reader`, never on a connection currently held by a `ReaderGuard`. A connection
  mid-recycle (closed and reopened) briefly is not sitting in the queue for other callers
  to take; under sustained high concurrency this could transiently reduce the effective
  reader count by one connection for the duration of a close+reopen (sub-millisecond,
  local file). Not expected to be observable under normal load; if it is, raise
  `reader_max_age` rather than lowering `max_readers`.
- **False WARN noise**: `truncate_high_water_pages` set too low relative to legitimately
  bursty write volume would fire TRUNCATE attempts (and their bounded stalls) during
  normal traffic spikes rather than genuine starvation. Mitigated by reusing the
  edge-triggered `crossing_warn` pattern for logging, and by the min-interval floor;
  tune `truncate_high_water_pages` upward if this is observed in practice rather than
  shortening `truncate_min_interval`.
- **Checkout-age watchdog false positives**: a legitimately slow but bounded query (e.g.
  a large `traverse` under heavy fan-out) could cross `KHIVE_READER_CHECKOUT_WARN_SECS`
  (10s) without indicating a leak. The watchdog only WARNs; it never forcibly reclaims a
  live checkout, so a false positive costs a log line, not correctness.

## Consequences

- WAL growth is now bounded on two independent axes: no single pooled reader connection
  can pin the tail for longer than `reader_max_age` (or `reader_max_ops` under sustained
  load), and once pressure sustains past `truncate_high_water_pages` regardless of cause,
  the daemon reclaims disk bytes via a bounded-blocking TRUNCATE rather than relying on
  an operator to notice a WARN and intervene manually.
- The existing periodic PASSIVE checkpoint tick, its skip-on-busy behavior, and its
  `warn_pages`/`high_water_pages` WARN semantics are unchanged. This ADR is additive to
  `checkpoint.rs`, not a rewrite.
- New operator-facing signal: an `oldest_checkout_age` WARN correlated with a WAL
  high-water WARN now points directly at a checkout being open for Ns, closing the
  diagnostic gap the incident exposed (the incident's `0|3768965|44` had no accompanying
  signal explaining which session or operation was responsible).
- Three new config knobs for Plank 1, three for Plank 2, all defaulting to conservative
  values requiring no operator action; existing deployments get the fix without
  environment changes.
- Follow-up (tracked separately, not blocking this ADR): instrument which MCP verb or
  handler is holding the oldest outstanding checkout (today's watermark is anonymous).
  This would need per-lease provenance tagging, a larger change than the global
  watermark proposed here.
