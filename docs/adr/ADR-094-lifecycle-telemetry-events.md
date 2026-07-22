# ADR-094: Sequencing-Assertable Lifecycle Telemetry Events

**Status**: Accepted
**Date**: 2026-07-04
**Depends on**: [ADR-041](./ADR-041-event-provenance-projection.md),
[ADR-091](./ADR-091-wal-snapshot-lifetime.md)

## Context

Human-readable tracing is useful for diagnosis but is not a durable, queryable contract.
Tests that capture tracing output also duplicate subscriber implementations and cannot
reliably assert ordering across asynchronous daemon work.

The public daemon and storage stack needs lifecycle facts that can be queried in causal
order. Two cases require durable events:

- the value chosen by a process-wide, first-read configuration singleton; and
- the outcome of a WAL checkpoint cycle while pressure is elevated, including the edge
  where pressure drains.

These facts belong in the existing event store. They do not justify another telemetry
table or a new request verb.

## Decision

### 1. Additive closed event kinds

`EventKind` remains a closed Rust enum. Its `ALL` constant, parser, formatter, and
round-trip tests must remain exhaustive. This ADR adds:

- `ConfigLocked`: emitted once when a process first fixes a cached configuration key and
  value.
- `CheckpointOutcomeRecorded`: emitted for each checkpoint cycle at or above the warning
  threshold and once more on the transition back below that threshold.

The SQL `kind` column is text, so adding enum variants requires no table migration.
Producers cannot invent ad hoc string categories.

### 2. Direct best-effort emission

Lifecycle producers append through `EventStore::append_event` directly. They do not
dispatch a verb to create telemetry. An append failure is logged and does not stop the
checkpoint task or fail the request that happens to drain configuration events.

`VerbRegistry` exposes a read-only clone of its configured event sink:

```rust
impl VerbRegistry {
    pub fn event_store(&self) -> Option<Arc<dyn EventStore>> {
        self.event_store.clone()
    }
}
```

The checkpoint task receives an optional `Arc<dyn EventStore>` and namespace through its
existing daemon construction path. The storage crate continues to depend only on the
storage trait, not on the runtime registry.

When no event store is configured, lifecycle emission is a no-op.

### 3. Configuration ledger

A synchronous `OnceLock` initializer cannot await an event append. It therefore records
`(key, value)` in a small process-wide pending ledger and sets an atomic pending flag.

The next `VerbRegistry::dispatch` call drains that ledger after it has access to the event
store. The fast path is one atomic flag check; the mutex is touched only when entries are
pending. `OnceLock` provides one enqueue per key, and draining uses an atomic take so a
queued entry is emitted at most once.

Emission remains best effort. Process termination between enqueue and drain can lose the
telemetry event. The ledger is observability state, not an input to configuration
correctness.

`ConfigLocked` payload:

```json
{
  "key": "default_output_format",
  "value": "json",
  "source": "runtime_config"
}
```

The draining request's identity is incidental. `source` identifies the component that
fixed the value.

### 4. Checkpoint sequencing

The checkpoint loop emits `CheckpointOutcomeRecorded`:

- on every cycle where `wal_pages >= warn_pages`; and
- exactly once when the state changes from at-or-above the threshold to below it.

The recovery row has `above_warn = false`. The loop reuses its existing
`was_above_warn` state:

```rust
let was_elevated = was_above_warn;
let crossed = crossing_warn(above_warn, &mut was_above_warn);
if above_warn {
    append_checkpoint_event(true, wal_pages, outcome).await;
} else if was_elevated {
    append_checkpoint_event(false, wal_pages, outcome).await;
}
```

The drain row distinguishes isolated threshold crossings from sustained elevation. For
example, three isolated cycles yield `true, false, true, false, true, false`; three
consecutive elevated cycles yield `true, true, true` until recovery. A windowed query can
therefore assert sustained pressure without treating old isolated crossings as consecutive.

Payloads include the threshold state, WAL pages, checkpoint mode, result class, frames
processed, and elapsed time already available to the checkpoint loop.

### 5. Ordering contract

Sequencing tests query the event store with a unique namespace and the relevant event
kinds, ordered by `(created_at, id)`. Persisted events, rather than captured tracing
records, are the assertion surface for lifecycle ordering.

Where two rows share a timestamp, UUID ordering provides a stable storage order but does
not manufacture causality. Producers that require causal ordering must append sequentially
and tests must assert the resulting order.

### 6. Volume and retention

`ConfigLocked` is bounded by the number of declared singleton keys per process.
`CheckpointOutcomeRecorded` emits nothing while healthy, emits once per elevated cycle,
and adds at most one recovery row per elevation episode.

This ADR does not add an event-retention policy. Edge-triggered healthy-state behavior and
bounded singleton emission prevent an unconditional per-tick baseline.

### 7. Non-goals

- No new storage substrate or table.
- No new public verb, resource, subscription, or notification.
- No removal of human-readable tracing.
- No claim that best-effort telemetry is a correctness dependency.
- No generic retention mechanism.

## Failure modes

- **Append failure**: log and continue the primary operation.
- **No event store**: skip emission.
- **Crash before configuration-ledger drain**: the pending row may be lost.
- **Daemon restart**: in-memory singleton and threshold-edge state begin a new process
  epoch; queries must not infer continuity across epochs without an explicit marker.
- **Unknown event kind in an older reader**: surface it as unknown rather than silently
  mapping it to another kind.

## Verification

Tests must cover:

- exhaustive `EventKind` parse/display round trips;
- one `ConfigLocked` enqueue under concurrent first readers;
- at-most-once ledger drain;
- no mutex access on the empty-ledger fast path;
- no configured event store;
- append failure that does not fail dispatch or checkpoint work;
- elevated sequences `true,true,true`;
- isolated sequences with interposed `false` recovery rows;
- exactly one recovery row per elevation episode; and
- persisted ordering by `(created_at, id)`.

## Alternatives considered

| Alternative                                              | Reason rejected                                                                           |
| -------------------------------------------------------- | ----------------------------------------------------------------------------------------- |
| Capture tracing output in tests                          | Duplicates subscribers and does not provide durable query semantics.                      |
| Emit a lifecycle verb                                    | Adds a caller-visible surface and a second audit row for internal telemetry.              |
| Use open string event kinds                              | Typos become silent categories without exhaustive compiler checks.                        |
| Emit checkpoint events on every healthy tick             | Creates an unnecessary unbounded baseline.                                                |
| Add a checkpoint cycle counter instead of a recovery row | Requires additional persisted state when the existing threshold-edge state is sufficient. |

## Consequences

### Positive

- Lifecycle sequencing is queryable and testable through the event store.
- Checkpoint pressure distinguishes sustained elevation from isolated crossings.
- Configuration singleton choices become visible with bounded event volume.

### Negative

- Best-effort emission can lose rows during process termination or storage failure.
- The runtime gains a small configuration ledger and event-store accessors.
- Event retention remains a separate unresolved policy.

## References

- [ADR-041](./ADR-041-event-provenance-projection.md): event projection and query context
- [ADR-091](./ADR-091-wal-snapshot-lifetime.md): checkpoint thresholds and escalation
