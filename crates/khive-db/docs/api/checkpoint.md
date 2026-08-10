# Checkpoint Task

The checkpoint task (`crates/khive-db/src/checkpoint.rs`) is a periodic
background `tokio::spawn` that keeps the SQLite WAL file from growing
unbounded and surfaces pressure/staleness signals to operators (ADR-091: WAL
checkpoint pressure telemetry + TRUNCATE escalation + tx-age sweep;
dedicated-checkpoint-connection amendment, 2026-08-02 — see the ADR's own
amendment section for the full rationale). Public item contracts stay
complete in their doc-comments; this file is the function-specific technical
reference for the private helpers, metrics surface, and the test suite that
pins down each guarantee.

## Module overview (ADR-091 Planks 0/1/2)

See `crates/khive-db/src/checkpoint.rs` module doc.

The periodic task issues `PRAGMA wal_checkpoint(PASSIVE)` on every tick,
including when the WAL page count exceeds the high-water mark. Ordinary
ticks stay PASSIVE-only and non-blocking; a rare, separately-gated escalation
may additionally run `PRAGMA wal_checkpoint(TRUNCATE)` on the same dedicated
connection with a shortened busy timeout (Plank 2, below).

**Non-contending design (2026-08-02 amendment — supersedes the original
`try_writer_nowait` design below).** The task opens its own dedicated,
long-lived standalone connection (`CheckpointConnection`, opened once at
task startup via `ConnectionPool::open_standalone_writer`) and runs PASSIVE
(and, when armed, TRUNCATE) on it every tick — `checkpoint_once` never
checks out the pool's writer mutex at all. `PRAGMA wal_checkpoint` takes
SQLite's CKPT lock, not the WRITE lock, so a concurrent pool writer can
commit while a checkpoint tick runs; serializing the two through one
application-level mutex, as the original design did, imposed contention
SQLite itself never required. Production evidence for the original design's
cost: 22 caller-side `pool.writer()` admission timeouts on 2026-08-02 across
the fleet, sustained bursts, against a persistent 64MiB WAL — a PASSIVE pass
over a WAL that large ran long enough to starve every concurrent writer for
the tick's duration. A tick is `Skipped` only when the dedicated connection
itself is unavailable (never opened yet, e.g. an in-memory or read-only
pool, or dropped after a prior tick's connection-level pragma failure, which
`CheckpointConnection::ensure_open` lazily reopens on the next tick) — a busy
pool writer no longer produces a Skipped tick at all; PASSIVE now runs
unconditionally on every tick regardless of concurrent write traffic.

_Original design (superseded above, kept for history):_ `checkpoint_once`
used `try_writer_nowait` (zero-wait `try_lock`) so a tick was skipped
immediately when any writer held the pool's writer mutex, rather than
blocking for up to `checkout_timeout`.

**Why TRUNCATE is excluded from every ordinary tick**: TRUNCATE inherits
RESTART semantics — it waits for active readers to release their WAL
snapshots and invokes the busy handler before acquiring the exclusive lock
needed to reset the WAL file. Blindly running it every tick could sit inside
SQLite holding the dedicated checkpoint connection for the length of its
(shortened) `truncate_busy_timeout`. PASSIVE never waits for readers; it
checkpoints as many frames as currently possible and returns promptly. When
WAL pressure is sustained (`high_water_pages` exceeded), the task emits a
WARNING; once WAL pressure reaches the much higher
`truncate_high_water_pages` mark, the rare Plank 2 escalation may
additionally attempt a bounded, rate-limited TRUNCATE under a deliberately
shortened busy timeout — replacing what used to be a purely
operator-scheduled manual step. Because TRUNCATE also now runs on the
dedicated connection rather than the pool writer, a concurrent
`pool.writer()` checkout is no longer queued behind it — that is an
ADMISSION-only guarantee. At the SQLite level TRUNCATE still additionally
acquires SQLite's writer lock (RESTART semantics plus TRUNCATE's own
exclusive step) and blocks new write transactions on every connection, this
process or any other, for up to `truncate_busy_timeout` while it waits on a
pinning reader — that bounded blocking window is unchanged by this
connection split; see the ADR-091 Amendment 5 dedicated-connection section
for the full admission-vs-SQLite-lock distinction.

**Threshold-crossing WARN semantics**: both the `warn_pages` and
`high_water_pages` warnings fire at most once per below→above crossing.
Skipped ticks (dedicated connection unavailable) leave the crossing state
unchanged so that a skip cannot spuriously re-arm the rate limit while WAL
pressure is still elevated. The ADR-091 Plank 0 open-transaction-registry
WARNs (oldest-entry escalation and the high-water snapshot enumeration) ride
the SAME crossing gates — they are not independently rate-limited, so they
never repeat on consecutive ticks that remain above a threshold. Only the
per-tick `debug!` trace of the oldest open entry, and the Plank 1 age sweep,
run unconditionally on every tick — including a Skipped one; the sweep's own
emissions stay edge-triggered per rung via `TxAgeSweepState`.

### Plank 2: rare TRUNCATE escalation

The periodic tick stays PASSIVE-only and non-blocking; on top of it,
`checkpoint_once` also evaluates a much rarer escalation to `PRAGMA
wal_checkpoint(TRUNCATE)` once the WAL has grown past
`truncate_high_water_pages` and at least `truncate_min_interval` has elapsed
since the last TRUNCATE _attempt_ (not the last successful reclaim).

This runs on the **same dedicated connection** `checkpoint_once` already
holds — there is never a second connection or a pool checkout for TRUNCATE.
If the dedicated connection is unavailable, both PASSIVE and any due
TRUNCATE are skipped for that tick, and `last_truncate_attempt` is left
untouched so the next tick where the connection is available is immediately
eligible rather than waiting out the full interval again.
`last_truncate_attempt` only advances on a tick that actually attempted
TRUNCATE (connection available, threshold crossed, interval elapsed) — never
on a skip for any reason (connection unavailable, below threshold, interval
not yet up).

TRUNCATE runs under a temporarily shortened `busy_timeout`
(`truncate_busy_timeout`), restored on the dedicated connection immediately
after the attempt, win or lose. No transaction is ever killed or aborted
here — the tx_registry is only read for diagnostics; Plank 1 owns the
registry's own bound.

### Plank 1: age-based background sweep, not per-statement rejection

The ADR's original text describes a "cooperative stale-op guard" that
rejects further statements and rolls back a late `commit()` on a
`SqliteTransaction`/`begin_tx` span past `KHIVE_TX_MAX_AGE_SECS`. That API no
longer exists in this codebase: ADR-067's `atomic_unit` replaced every
production write-transaction path with a closure that structurally cannot
outlive its own call (single-poll-enforced on the write-queue path), which
is the ADR's own named follow-up ("closure-scoped transaction API"), already
delivered for writes by a later ADR.

What ships here instead is the part of Plank 1 that still applies to every
registered span regardless of which mechanism created it: on EVERY tick —
Skipped as well as Observed, since the sweep must not go blind for the
duration of a Skipped outage (dedicated connection unavailable) any more
than for an ordinary busy tick — `TxAgeSweepState`
checks `khive_storage::tx_registry::oldest()`'s age against
`tx_warn_secs`/`tx_max_age_secs` and escalates to `warn!`/`error!` on each
below→above crossing (same debounce idiom as the WAL-pressure ladder — a
sustained stale span logs once per rung, not once per tick), also re-arming
both rungs if the oldest entry's identity changes between ticks so a
departed span's latched state cannot suppress its replacement. This is
visibility, not reclamation: nothing here force-closes a stale span,
matching the ADR's own accepted gap for a transaction "held idle across an
await with no further calls."

## Metrics read-surface (load/perf harness)

See `crates/khive-db/src/checkpoint.rs` — the module-scoped `AtomicU64`
compatibility counters (`LAST_WAL_PAGES`, `TRUNCATE_ATTEMPTS`, etc.) plus the
backend-keyed `RoutineWalObservation` registry.

Mirrors the fallback-counter pattern in `khive-mcp/src/daemon.rs`
(`FALLBACK_*` statics + their `pub(crate)` accessors): the checkpoint task is
a single fire-and-forget `tokio::spawn` with no handle retained anywhere the
daemon's connection-accept loop can reach, so these are plain module-scoped
atomics rather than a struct threaded through every `checkpoint_once`/
`maybe_truncate`/`note_truncate_outcome` call site (and, transitively, every
existing test call site). Read-only surface: nothing here is ever reset
outside `#[cfg(test)]`, and nothing reachable over the daemon wire can reset
them either (see `khive_runtime::daemon::DaemonRequestFrame::metrics_only`).

Each periodic tick issues exactly one routine `PRAGMA wal_checkpoint(PASSIVE)`
and retains that row's `busy`, `log`, and `checkpointed` values. Logical
backlog is `max(log - checkpointed, 0)`. The same tick stats the backend's
physical `-wal` sidecar and records its byte allocation independently. This
matters because PASSIVE can drain every logical frame while SQLite retains the
sidecar's high-water allocation for reuse. `routine_wal_observation(pool)` is
a pure backend-scoped memory read: a metrics scrape performs neither another
checkpoint nor another filesystem stat, and a secondary backend cannot win a
process-global race and masquerade as the main backend's sample (#1849).

## `run_checkpoint_task` — shutdown design history

See `crates/khive-db/src/checkpoint.rs` — `run_checkpoint_task`.

An earlier version of this task used `Arc::strong_count(&pool) <= 1` as its
exit condition instead of an explicit signal. That check is unreachable
whenever a sibling owner holds its own clone of `pool` for the task's
lifetime — which the production boot path does: `CheckpointLifecycleOwner`
contains a `SqlEventStore` that retains its own `Arc::clone` of the same pool,
so the task always observed `strong_count == 2` and never exited via that
mechanism (issue #774). The explicit `watch` channel does not depend on how
many other owners exist.

Lifecycle ownership is independent of backend role. Spawn-time fan-out gives
the lifecycle owner to the main checkpoint task when one exists; if the main
backend is in-memory and only secondary file-backed tasks are spawned, the
first secondary task owns emission. Other tasks receive no owner, so the API
cannot silently discard a caller-supplied event store based on `is_main`.

Lifecycle persistence is isolated from the scheduler (issue #1434). The task
serializes each outcome and uses a zero-wait `try_send` into a dedicated append
worker with capacity for one queued event behind the append in progress. If
both slots are occupied, that outcome is dropped; the first drop in each
uninterrupted full-queue episode warns, and a later successful enqueue re-arms
the warning. The worker logs sink failures and continues. This preserves
accepted-event order and bounds work and memory under writer contention while
ensuring event-store checkout or SQLite busy waits cannot delay a later
PASSIVE/TRUNCATE cycle. Checkpoint-task shutdown aborts the scheduler-owned
async worker instead of waiting for its current append future. That guarantee
is local to `run_checkpoint_task`, not daemon, runtime, or process shutdown: if
the event store already admitted the append to `spawn_blocking` or a
`WriterTask`, aborting the worker does not cancel it, and at most one such sink
operation may outlive the checkpoint task. If a recovery/drain row meets a full
queue, the task keeps the elevation episode open and retries that drain on the
next healthy tick; a dropped enqueue therefore cannot permanently suppress the
row that closes a previously accepted elevated sequence.

## Private tx-registry logging helpers (Plank 0)

See `crates/khive-db/src/checkpoint.rs` — `log_tx_registry_oldest_debug`,
`log_tx_registry_oldest_warn`, `log_tx_registry_snapshot_warn`.

All three read `khive_storage::tx_registry` and are observe-only — none
enforces or force-closes anything. `log_tx_registry_oldest_debug` runs
unconditionally every tick (unrated-limited, debug-level). The two `warn!`
variants (`log_tx_registry_oldest_warn` for the single oldest entry,
`log_tx_registry_snapshot_warn` to enumerate every open entry — the "which
caller is holding the pin" answer ADR-091's static reading could not
produce) are NOT internally rate-limited: callers MUST gate them on a
below→above crossing (`warn_pages` / `high_water_pages` respectively, via
`crossing_warn`) or they reproduce the log-spam bug this rewrite fixes.

## `maybe_truncate` — TRUNCATE attempt gating (Plank 2)

See `crates/khive-db/src/checkpoint.rs` — private fn `maybe_truncate`.

Runs on the same dedicated checkpoint connection the caller already holds —
never performs its own checkout, and never touches the pool's writer mutex.
No-ops unless BOTH `wal_pages >= truncate_high_water_pages` AND no prior
attempt or `truncate_min_interval` has elapsed since the last one.
`truncate_state.last_attempt` is stamped ONLY immediately before the
TRUNCATE pragma itself runs (connection available, threshold crossed,
interval elapsed, AND the temporary busy_timeout override successfully
applied) —
every earlier return is a skip, not an attempt, and never touches it,
matching the ADR's "skip must not stamp" requirement. The oldest-pinning
snapshot is logged (reusing Plank 0's `tx_registry`) before the attempt.
`busy_timeout` is temporarily lowered to `truncate_busy_timeout` for the
PRAGMA call and restored immediately after, regardless of outcome. No
transaction is ever killed here — enforcement is Plank 1's job, not this
one's. The OS holder census is captured immediately before the TRUNCATE
attempt, then consumed only if that attempt makes no progress. The reported
identities therefore include a transient holder that releases during the
bounded TRUNCATE wait, before the no-progress diagnostic is emitted. Sidecar
enumeration remains deferred to the no-progress path.

## `TxAgeSweepState` — identity tracking rationale

See `crates/khive-db/src/checkpoint.rs` — `TxAgeSweepState`, `TxAgeSweepState::observe`.

`tx_registry::oldest()` can be pinned by any registered span regardless of
which call site created it (`atomic_unit`, `WriterGuard::transaction`, a
store's own batch-upsert helper, or one of `graph.rs`'s bounded,
statement-scoped traversal reads). This is deliberately a different signal
from the WAL-pressure ladder: a registered span can go stale while `wal_pages` sits well under
`warn_pages` (nothing has pinned the checkpoint boundary yet), and
conversely `wal_pages` can be elevated with an empty registry (the pin, if
any, is outside this process — see the ADR's own "route reads through the
daemon" alternative, out of scope here).

`observe` resets both latches on a below-threshold (or absent) oldest entry
so a future stale span can re-arm both rungs. Identity is tracked
separately: if the oldest entry's `TxId` differs from the previous tick's,
both latches are force-reset before re-evaluating the new entry's age, even
though this happens on the SAME tick as the age check (not a separate
below-threshold tick in between). Without this, an already-latched-`true`
state from the _departed_ entry would silently suppress the crossing for a
_different_ span that replaced it while already stale — naming nobody at
exactly the moment a new long-lived span starts pinning the database, which
is the scenario this sweep exists to catch. A merely fresher replacement
still reads as below-threshold either way, so the identity check changes
behavior only for an already-stale successor.

## Test coverage notes

Regression and edge-case rationale for the tests in `checkpoint.rs`'s
`#[cfg(test)]` module. Test code doesn't render on docs.rs, so in-source
comments stay short; the full "why" — what incident or edge case each
test guards — lives here.

### `log_tx_registry_oldest_debug_reports_oldest_open_entry`

`#[serial(tx_registry)]`: the registry is a process-wide singleton shared
across every test in this binary — see `pool.rs`'s and `sql_bridge.rs`'s
registry tests, which share this same serial group (these three were
previously unserialized and could race, corrupting each other's
`oldest()`/`snapshot()` reads).

This test does NOT hardcode "checkpoint_tick_test" as the expected label:
production write paths elsewhere in this same test binary (vectors/graph/text
stores) also register short-lived registry entries while their own tests
run, and `serial(tx_registry)` only serializes against the OTHER tests in
that same group, not against every write path in the crate. Instead it
samples `oldest()` itself immediately before invoking the function under
test and asserts the logged label matches whatever the registry considers
oldest at that instant — deterministic regardless of unrelated concurrent
registry churn, while still verifying `log_tx_registry_oldest_debug`
correctly surfaces the registry's own `oldest()` answer.

### `checkpoint_task_exits_via_shutdown_signal_with_live_event_store_pool_clone`

Regression for issue #774: on the production boot path, the daemon passes
`run_checkpoint_task` both `pool` directly and an `event_store` that
internally retains its own `Arc::clone` of the same pool
(`SqlEventStore::new_scoped`). A strong-count-based exit condition can never
fire in that shape, because the task always observes at least two live
clones — its own `pool` argument plus the one buried in `event_store`. This
test reproduces that exact ownership shape (a real `SqlEventStore` holding a
sibling clone) and asserts the task still exits promptly via the
watch-channel signal, proving the fix does not depend on `Arc::strong_count`
at all.

### `checkpoint_high_water_does_not_block_behind_reader`

Regression: a high-water tick must NOT block behind an active read
transaction.

Isomorphism guarantee: this test FAILS if `checkpoint_once` regresses to
`PRAGMA wal_checkpoint(TRUNCATE)`. Confirmed by reasoning: TRUNCATE inherits
RESTART semantics and will invoke the busy handler (sleeping up to
`busy_timeout`) while waiting for the open reader snapshot to release. With
`busy_timeout = 2000ms` a TRUNCATE regression causes the call to take
~2000ms, blowing the <500ms assertion. PASSIVE returns in <1ms even with an
open reader, because PASSIVE never waits for readers.

Why `busy_timeout = 2000ms` and threshold `< 500ms`: the original 200ms
busy_timeout / 50ms threshold was too tight for contended CI runners where
PASSIVE legitimately takes 50-200ms under parallel-test load. Raising the
busy_timeout to 2000ms keeps the PASSIVE path well below 500ms while a
TRUNCATE regression blocks for ~2000ms — a 4x safety margin on both sides.

An idle reader connection (no `BEGIN`) does NOT pin frames and would not
cause TRUNCATE to wait — an actual open read transaction is required for
the isomorphism to hold.

### `checkpoint_once_proceeds_and_can_attempt_truncate_while_pool_writer_held` (dedicated-connection fix, 2026-08-02)

The fix this module exists to prove, at the unit level: holding the POOL's
writer mutex (`pool.try_writer()`) must NOT cause `checkpoint_once` to skip
or block — PASSIVE (and, if armed, TRUNCATE) run on the task's own dedicated
connection, which never contends with the pool writer at all. Before the
fix, this exact setup made `checkpoint_once` return `Skipped` via
`try_writer_nowait()`. Replaces the old `busy_writer_skips_both_passive_and_truncate`
test, whose entire premise (a busy pool writer causes a skip) is no longer
true.

See `crates/khive-db/tests/checkpoint_dedicated_connection.rs` for the
integration-level companion: a real `checkpoint_once` call against a
genuinely fat (≥32MiB) WAL, run concurrently with a `pool.writer()`
admission attempt, proving the converse direction — the writer is never
blocked behind the checkpoint either.

### `open_standalone_writer_fails_on_in_memory_pool`

An in-memory pool has no on-disk file for `ConnectionPool::open_standalone_writer`
to open a second connection against. This is exactly the precondition that
makes `CheckpointConnection::ensure_open` return `None` and
`run_checkpoint_task` report `Skipped` — `checkpoint_once` itself is never
even called for an in-memory pool. Replaces the old
`checkpoint_once_is_noop_on_in_memory_pool` test, which called
`checkpoint_once` directly against an in-memory pool — no longer possible
once `checkpoint_once` takes an already-open dedicated connection as an
argument rather than acquiring one itself.

### `checkpoint_task_sweeps_stale_entry_even_when_dedicated_connection_is_unavailable_every_tick`

Renamed and re-mechanized from
`checkpoint_task_sweeps_stale_entry_even_when_writer_is_busy_every_tick`:
since the dedicated-connection fix, holding the pool's writer mutex no
longer produces a Skipped tick at all, so the original mechanism (hold
`pool.try_writer()` for the task's whole run) can no longer drive the
Skipped path this regression exists to cover. Drives Skipped the way it
actually happens now instead: a read-only pool, on which
`ConnectionPool::open_standalone_writer` always fails, so
`CheckpointConnection::ensure_open` can never open a dedicated connection.
The underlying regression this test guards — the Plank 1 age sweep must
still run and escalate on a Skipped tick, not just an Observed one — is
unchanged; only the mechanism that produces the Skipped tick moved.

### `checkpoint_config_rejects_reversed_tx_thresholds`

Fix: a reversed pair — `KHIVE_TX_WARN_SECS` >= `KHIVE_TX_MAX_AGE_SECS` —
must not be honored independently. Before this fix, WARN_SECS=120 /
MAX_AGE_SECS=30 parsed both values successfully (each is independently
positive) and produced a sweep that emits `Stale` at 30s while never
reaching the `Warn` crossing until 120s — inverting the intended severity
ladder. Both thresholds must instead fall back to their defaults together.

### `checkpoint_config_rejects_equal_tx_thresholds`

Same invariant, the degenerate equal case: WARN_SECS == MAX_AGE_SECS would
make an entry cross both rungs on the exact same tick every time,
collapsing the two-rung severity ladder into one. Must also fall back to
defaults, not merely reject a strictly-reversed pair.

### `skipped_tick_does_not_reset_high_water_crossing_state`

Regression: a Skipped tick must NOT reset `was_above_high_water`.

Before the fix, `checkpoint_once` returned `0` on both a genuinely-empty WAL
and a writer-busy skip. The task treated `0` as an observed page count and
reset `was_above_high_water`, re-arming the rate limit on every busy tick.
With the fix, `CheckpointTick::Skipped` leaves crossing state unchanged.

This test drives `crossing_warn` directly (the pure function that owns the
decision) rather than going through the async task, which would require a
logging harness.

### `tx_age_sweep_stale_replacement_without_intervening_clear_still_names_new_entry`

Fix: a stale entry (A) that closes and is immediately replaced by an
ALREADY-stale entry (B) on the very next observed tick — no intervening
below-threshold or empty tick, unlike `tx_age_sweep_rearms_after_entry_clears`
— must still emit both rungs for B. Before the identity-tracking fix,
`was_above_warn` and `was_above_max_age` were already `true` from A, so B's
crossing was silently swallowed: the alert stayed latched to a departed
caller while a different long-lived span was now pinning the database.

### `tx_age_sweep_uses_configured_thresholds_not_hardcoded_defaults`

`KHIVE_TX_WARN_SECS` / `KHIVE_TX_MAX_AGE_SECS` are read into the config via
`from_env` at `run_checkpoint_task` construction time, so this closes the
loop from env var to the actual emitted rung (the earlier
`checkpoint_config_env_override` test only asserts the config fields
themselves).

### `tx_age_sweep_names_long_lived_reader_pinning_wal_past_high_water`

Integration-level regression for the incident this ADR fixes: a real `BEGIN
DEFERRED` reader pins a WAL snapshot (exactly like
`checkpoint_high_water_does_not_block_behind_reader`) while also being
registered in the shared `tx_registry` (simulating an instrumented
long-lived-reader call site; `graph.rs`'s `graph_traverse_read` is no longer
such a site after ADR-091 Amendment 4), writes drive `wal_pages` past
`high_water_pages`, and — with a
millisecond-scale `tx_max_age_secs` so the test does not sleep for real
minutes — the Plank 1 sweep escalates to `Stale` naming that exact reader,
alongside the existing Plank 0 high-water WARN. This is the "detection
works, mitigation missing" gap from the incident: the sweep now gives the
operator the specific, escalating, un-silenced signal that a single
one-shot high-water WARN does not.

### `tx_age_sweep_own_entry_survives_concurrent_older_registration`

Regression for #926: reproduces the exact race that made
`tx_age_sweep_names_long_lived_reader_pinning_wal_past_high_water` flaky,
directly rather than hoping cargo's test scheduler happens to interleave two
unrelated tests. `tx_registry` is a process-wide singleton; a decoy entry
registered before this test's own entry is genuinely older, so raw
`oldest()` cannot return the test fixture — exactly what an unrelated,
concurrently-running write path (e.g. `graph_upsert_edges`) could do in the
real suite. The fix (looking up this test's own entry by label via
`snapshot()` instead of trusting global `oldest()`) must still correctly
name and escalate THIS entry despite that older decoy.

### Healthy-tick WAL-pin sidecar collection

After the task refreshes its own WAL-pin beacon/heartbeat, every
sidecar-enabled checkpoint tick that did not already run no-progress
attribution runs one bounded `walpin::housekeep_live` pass. It removes only
positively dead/reused-PID residue and preserves malformed, uninspectable, and
live-but-stale evidence for attribution. Legacy records with no declared
producer cadence use the session-sweep fallback captured once at task startup
(the ADR-defined compiled default, 5000 ms), never the daemon's checkpoint
interval or the enumerator's current environment override.

The TRUNCATE-no-progress path instead runs the destructive attribution
enumerator. The synchronous checkpoint core only records a request containing
the pre-TRUNCATE holder census and stable sidecar inputs. The async task then
executes the directory walk in an awaited `spawn_blocking` before it considers
ordinary housekeeping or emits the tick's lifecycle outcome; report logging
cannot consume a partial result. It marks the tick attempted before spawning,
so a successful pass, a trust-boundary enumeration error, or a blocking-worker
join failure all suppress same-tick housekeeping. This is fail-safe for the
one-pass bound because a failed worker may already have completed part of the
walk. Enumeration and worker failures retain distinct classifications and
warn without stopping the task. There is therefore at most one
sidecar-directory pass per tick, processing at most `MAX_SIDECAR_ENTRIES`,
including on a no-progress tick. Collection does not depend on WAL pressure or
checkpoint availability; explicitly disabling the sidecar disables it.
