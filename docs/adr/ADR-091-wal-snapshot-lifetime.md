# ADR-091: Bounded Read-Transaction Lifetime and WAL Checkpoint Escalation

**Status**: Accepted (ratified 2026-07-05)
**Date**: 2026-07-04
**Depends on**: [ADR-015](./ADR-015-schema-migrations.md),
[ADR-049](./ADR-049-khived-daemon.md),
[ADR-067](./ADR-067-write-owner-daemon.md)

## Context

In SQLite WAL mode, a live read transaction fixes an end mark. A checkpoint cannot reclaim
frames newer than the oldest active end mark. Sustained pins can therefore allow the WAL to
grow even when the writer itself is not locked.

Two invariants define the basis of this ADR:

1. **An idle autocommit connection does not pin a WAL snapshot.** A read transaction ends
   when its statement finishes and resets. A regression test must issue `BEGIN DEFERRED`
   and perform a read to construct a real pin.
2. **File-backed read transactions are closure-scoped and instrumented directly.** Recycling
   returned `ReaderGuard` values remains test-pool hygiene and is not part of the file-backed
   WAL policy.

Every runtime-managed transaction span is registered from successful `BEGIN` through its
terminal commit, rollback, or drop path. Checkpoint escalation mitigates WAL pressure but does
not replace the transaction-lifetime bound or the regression tests that enforce it.

## Decision

The WAL policy has three planks:

1. observe all runtime-registered transaction spans;
2. make file-backed writes closure-scoped and report stale spans; and
3. escalate from ordinary PASSIVE checkpointing to a bounded, infrequent TRUNCATE attempt.

### Plank 0: transaction and checkpoint instrumentation

The runtime maintains a process-local open-transaction registry:

```rust
struct TxMeta {
    id: TxId,
    opened_at: Instant,
    label: Option<String>,
}
```

Every instrumented read or write transaction registers immediately after `BEGIN` succeeds
and deregisters on every commit, rollback, and drop path. Existing operation labels are
reused where available.

On every checkpoint tick, including ticks where writer acquisition is skipped, the age
sweep inspects the oldest registry entry. It emits an edge-triggered warning when the entry
crosses `KHIVE_TX_WARN_SECS` and an error when it crosses
`KHIVE_TX_MAX_AGE_SECS`.

The sweep tracks both age and transaction identity. If one stale entry closes and another
already-stale entry becomes oldest, the new identity re-arms both severity edges.

When a TRUNCATE attempt makes no progress, diagnostics include each open entry's identifier,
age, and optional label. They never assert that an unregistered external process is absent;
the registry is process-local.

### Plank 1: bounded construction and stale-span visibility

File-backed writes use the single-writer `atomic_unit` closure from ADR-067. The closure is
executed within the writer task's transaction and cannot be retained by its caller across an
await. This is the structural lifetime bound for writes.

The shipped stale-age policy is observability, not cross-task cancellation:

- `KHIVE_TX_WARN_SECS` defaults to 30 seconds;
- `KHIVE_TX_MAX_AGE_SECS` defaults to 120 seconds;
- crossings are logged once per transaction identity and severity rung; and
- the sweep never force-closes a connection owned by another task.

No per-statement rejection or commit-time forced rollback is part of the shipped decision.
Those mechanisms would not cover a transaction held idle without another statement, and
unsafe cross-task connection manipulation is prohibited.

A long read closure can still remain open while processing a bounded chunk sequence. Such
reads must register and label their transaction so the sweep makes them visible. Aborting a
read at an age threshold, returning partial results, or restructuring traversal is a
separate API decision.

The legacy returned-reader age and operation-count recycling remains limited to in-memory
and test pools:

- `KHIVE_READER_MAX_AGE_SECS=300`;
- `KHIVE_READER_MAX_OPS=5000`; and
- `KHIVE_READER_CHECKOUT_WARN_SECS=10`.

These keys must not be described as protection for the file-backed read
path.

### Plank 2: checkpoint escalation

Ordinary ticks retain the existing behavior:

- acquire the writer without waiting;
- skip the tick if the writer is busy;
- run `PRAGMA wal_checkpoint(PASSIVE)`; and
- never block normal writers for routine observation.

The task adds a rarer escalation:

| Setting                                | Default | Meaning                                   |
| -------------------------------------- | ------: | ----------------------------------------- |
| `KHIVE_WAL_TRUNCATE_HIGH_WATER_PAGES`  |   20000 | WAL frame count that arms TRUNCATE.       |
| `KHIVE_WAL_TRUNCATE_MIN_INTERVAL_SECS` |     300 | Minimum interval between actual attempts. |
| `KHIVE_WAL_TRUNCATE_BUSY_MS`           |    2000 | Temporary busy timeout for an attempt.    |

When the high-water threshold is reached and the interval has elapsed, a tick that acquires
the writer runs PASSIVE first and then attempts
`PRAGMA wal_checkpoint(TRUNCATE)`. The temporary busy timeout is restored on every exit
path.

If writer acquisition fails, the task does not spin or retry inside the tick.
`last_truncate_attempt` advances only when the task actually acquires the writer and makes
an attempt. The next free tick is therefore immediately eligible after a skipped attempt.

One writer checkout is used per tick. If the writer remains continuously busy, TRUNCATE may
never run and WAL growth may continue. The age sweep and lifecycle telemetry make this
state visible; the ADR does not claim guaranteed reclamation.

### Severity ladder

Checkpoint telemetry distinguishes:

1. **INFO**: first crossing of the warning threshold;
2. **WARN**: sustained failure to drain across a configured consecutive-cycle window; and
3. **ALARM**: crossing the TRUNCATE high-water threshold and arming escalation.

The recovery event defined by ADR-094 prevents isolated crossings from being treated as
consecutive. `wal_pages` is the instantaneous WAL frame count returned by the checkpoint
pragma, not a cumulative counter.

### Configuration summary

| Key                                    | Default | Scope                                |
| -------------------------------------- | ------: | ------------------------------------ |
| `KHIVE_TX_WARN_SECS`                   |      30 | Registry age warning.                |
| `KHIVE_TX_MAX_AGE_SECS`                |     120 | Registry age error; visibility only. |
| `KHIVE_READER_MAX_AGE_SECS`            |     300 | Returned test-pool reader recycling. |
| `KHIVE_READER_MAX_OPS`                 |    5000 | Returned test-pool reader recycling. |
| `KHIVE_READER_CHECKOUT_WARN_SECS`      |      10 | Test-pool checkout warning.          |
| `KHIVE_WAL_TRUNCATE_HIGH_WATER_PAGES`  |   20000 | TRUNCATE trigger.                    |
| `KHIVE_WAL_TRUNCATE_MIN_INTERVAL_SECS` |     300 | Attempt spacing.                     |
| `KHIVE_WAL_TRUNCATE_BUSY_MS`           |    2000 | Temporary attempt timeout.           |

The first two defaults are conservative starting values and require measurement before
tuning.

## Invariants

- Autocommit idle time is not counted as an open transaction.
- Registration begins only after `BEGIN` succeeds.
- Every terminal path deregisters exactly once.
- The age sweep runs before any skipped-checkpoint early return.
- A new oldest transaction identity re-arms severity reporting.
- File-backed write transactions cannot escape their `atomic_unit` closure.
- Checkpoint telemetry never force-closes a foreign task's connection.
- A TRUNCATE skip does not consume the minimum-attempt interval.
- The temporary busy timeout is always restored.

## Verification

Tests must cover:

- an idle autocommit connection does not block checkpoint progress;
- an explicit read transaction does block progress until rollback or close;
- registration and deregistration on commit, rollback, error, panic, and drop;
- age warnings on observed and skipped checkpoint ticks;
- severity re-arming when the oldest identity changes;
- no repeated log on a sustained unchanged identity;
- a write transaction cannot outlive its `atomic_unit` closure;
- PASSIVE behavior below the high-water threshold;
- TRUNCATE attempt and minimum-interval enforcement;
- skipped writer acquisition leaving the next tick eligible;
- busy-timeout restoration on success and failure;
- recovery rows separating isolated warning crossings; and
- legacy reader recycling limited to the in-memory/test path.

## Failure modes

- **Unregistered transaction**: invisible to the age sweep. Code review and transaction
  wrapper tests must keep registration coverage exhaustive.
- **Transaction in another process**: not visible in this process's registry. Checkpoint
  telemetry can show lack of progress but cannot attribute it locally.
- **Long read span**: reported but not cancelled. The caller may continue to pin the WAL
  until it completes.
- **Continuous writer activity**: both PASSIVE observation and TRUNCATE attempts can be
  skipped; the task must not spin.
- **Legitimate long transaction**: produces warning/error telemetry; thresholds may be
  raised only with measured evidence.
- **Event append failure**: checkpoint behavior continues and human-readable tracing remains
  available.

## Alternatives considered

| Alternative                                  | Reason rejected                                                                                                  |
| -------------------------------------------- | ---------------------------------------------------------------------------------------------------------------- |
| Recycle idle file-backed connections         | Idle autocommit connections do not hold snapshots, and the targeted pool path is not used for file-backed reads. |
| Force-close stale connections from the sweep | Unsafe while another task owns the connection and may violate transaction semantics.                             |
| Kill old client processes                    | Disruptive and does not identify or structurally prevent the pin.                                                |
| External checkpointer                        | Adds an operational process and IPC when the daemon already owns checkpoint work.                                |
| Change journal mode                          | Avoids addressing transaction lifetime and is not supported by the current stable SQLite dependency.             |
| Route all reads through one daemon           | A broader transport and topology change; it does not remove the need to bound transactions.                      |

## Consequences

### Positive

- The design distinguishes idle connections from open transactions.
- Every registered transaction span has age and identity visibility.
- Closure-scoped writes cannot be held across caller awaits.
- TRUNCATE escalation is bounded and does not alter routine PASSIVE behavior.

### Negative

- The registry cannot see another process or native work that bypasses registration.
- Stale read spans are observed but not forcibly reclaimed.
- Under continuous writer activity, escalation may not run.
- Threshold tuning requires reproducible workload measurements.

## References

- [ADR-015](./ADR-015-schema-migrations.md): schema evolution
- [ADR-049](./ADR-049-khived-daemon.md): daemon-owned checkpoint task
- [ADR-067](./ADR-067-write-owner-daemon.md): closure-scoped single-writer operations
- [ADR-094](./ADR-094-lifecycle-telemetry-events.md): checkpoint sequencing events
