# Checkpoint task internals

Long-form rationale extracted from `src/checkpoint.rs` doc-comments (ADR-091:
WAL checkpoint pressure telemetry + TRUNCATE escalation + tx-age sweep).
Public-item contracts stay complete in the source; this file carries the
"why", design history, and cross-references.

## Module overview (ADR-091 Planks 0/1/2)

See `crates/khive-db/src/checkpoint.rs` module doc.

The periodic task issues `PRAGMA wal_checkpoint(PASSIVE)` on every tick,
including when the WAL page count exceeds the high-water mark. Ordinary
ticks stay PASSIVE-only and non-blocking; a rare, separately-gated escalation
may additionally run `PRAGMA wal_checkpoint(TRUNCATE)` under the same writer
guard with a shortened busy timeout (Plank 2, below).

**Non-contending design**: `checkpoint_once` uses `try_writer_nowait`
(zero-wait `try_lock`) so a tick is skipped immediately when any writer holds
the mutex, rather than blocking for up to `checkout_timeout`. The checkpoint
task must never stall active write traffic — a skipped tick is always
preferable.

**Why TRUNCATE is excluded from every ordinary tick**: TRUNCATE inherits
RESTART semantics — it waits for active readers to release their WAL
snapshots and invokes the busy handler before acquiring the exclusive lock
needed to reset the WAL file. With `PoolConfig`'s 30s `busy_timeout`, blindly
running it every tick could sit inside SQLite holding the sole writer
connection for up to 30s, stalling all normal write traffic. PASSIVE never
waits for readers; it checkpoints as many frames as currently possible and
returns promptly. When WAL pressure is sustained (`high_water_pages`
exceeded), the task emits a WARNING; once WAL pressure reaches the much
higher `truncate_high_water_pages` mark, the rare Plank 2 escalation may
additionally attempt a bounded, rate-limited TRUNCATE under a deliberately
shortened busy timeout — replacing what used to be a purely
operator-scheduled manual step.

**Threshold-crossing WARN semantics**: both the `warn_pages` and
`high_water_pages` warnings fire at most once per below→above crossing.
Skipped ticks (writer busy) leave the crossing state unchanged so that a
skip cannot spuriously re-arm the rate limit while WAL pressure is still
elevated. The ADR-091 Plank 0 open-transaction-registry WARNs (oldest-entry
escalation and the high-water snapshot enumeration) ride the SAME crossing
gates — they are not independently rate-limited, so they never repeat on
consecutive ticks that remain above a threshold. Only the per-tick `debug!`
trace of the oldest open entry, and the Plank 1 age sweep, run
unconditionally on every tick — including a Skipped one; the sweep's own
emissions stay edge-triggered per rung via `TxAgeSweepState`.

### Plank 2: rare TRUNCATE escalation

The periodic tick stays PASSIVE-only and non-blocking; on top of it,
`checkpoint_once` also evaluates a much rarer escalation to `PRAGMA
wal_checkpoint(TRUNCATE)` once the WAL has grown past
`truncate_high_water_pages` and at least `truncate_min_interval` has elapsed
since the last TRUNCATE *attempt* (not the last successful reclaim).

This is a **single writer checkout per tick**: PASSIVE and any due TRUNCATE
both run under the one guard `checkpoint_once` already holds — there is
never a second concurrent checkout for TRUNCATE. If the writer mutex is
busy, both PASSIVE and any due TRUNCATE are skipped for that tick, and
`last_truncate_attempt` is left untouched so the next tick where the writer
is free is immediately eligible rather than waiting out the full interval
again. `last_truncate_attempt` only advances on a tick that actually
attempted TRUNCATE (writer held, threshold crossed, interval elapsed) —
never on a skip for any reason (writer busy, below threshold, interval not
yet up).

TRUNCATE runs under a temporarily shortened `busy_timeout`
(`truncate_busy_timeout`), restored on the writer connection immediately
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
Skipped as well as Observed, since a registered `WriterGuard::transaction`
span holds the writer mutex for its whole lifetime and would otherwise make
the busiest, most relevant tick invisible to this sweep — `TxAgeSweepState`
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
statics (`LAST_WAL_PAGES`, `TRUNCATE_ATTEMPTS`, etc.) and their accessors.

Mirrors the fallback-counter pattern in `khive-mcp/src/daemon.rs`
(`FALLBACK_*` statics + their `pub(crate)` accessors): the checkpoint task is
a single fire-and-forget `tokio::spawn` with no handle retained anywhere the
daemon's connection-accept loop can reach, so these are plain module-scoped
atomics rather than a struct threaded through every `checkpoint_once`/
`maybe_truncate`/`note_truncate_outcome` call site (and, transitively, every
existing test call site). Read-only surface: nothing here is ever reset
outside `#[cfg(test)]`, and nothing reachable over the daemon wire can reset
them either (see `khive_runtime::daemon::DaemonRequestFrame::metrics_only`).

## `run_checkpoint_task` — shutdown design history

See `crates/khive-db/src/checkpoint.rs` — `run_checkpoint_task`.

An earlier version of this task used `Arc::strong_count(&pool) <= 1` as its
exit condition instead of an explicit signal. That check is unreachable
whenever a sibling owner holds its own clone of `pool` for the task's
lifetime — which the production boot path does: `event_store`
(`Option<Arc<dyn EventStore>>`), when `Some`, is a `SqlEventStore` that
retains its own `Arc::clone` of the same pool, so the task always observed
`strong_count == 2` and never exited via that mechanism (issue #774). The
explicit `watch` channel does not depend on how many other owners exist.

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

Runs under the writer guard the caller already holds — never performs its
own checkout. No-ops unless BOTH `wal_pages >= truncate_high_water_pages`
AND no prior attempt or `truncate_min_interval` has elapsed since the last
one. `truncate_state.last_attempt` is stamped ONLY immediately before the
TRUNCATE pragma itself runs (writer held, threshold crossed, interval
elapsed, AND the temporary busy_timeout override successfully applied) —
every earlier return is a skip, not an attempt, and never touches it,
matching the ADR's "skip must not stamp" requirement. The oldest-pinning
snapshot is logged (reusing Plank 0's `tx_registry`) before the attempt.
`busy_timeout` is temporarily lowered to `truncate_busy_timeout` for the
PRAGMA call and restored immediately after, regardless of outcome. No
transaction is ever killed here — enforcement is Plank 1's job, not this
one's.
