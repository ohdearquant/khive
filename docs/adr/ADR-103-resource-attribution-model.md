# ADR-103: Resource Attribution Model

**Status**: Proposed
**Date**: 2026-07-12
**Authors**: khive maintainers
**Depends on**: [ADR-016](./ADR-016-request-dsl.md),
[ADR-091](./ADR-091-wal-snapshot-lifetime.md),
[ADR-094](./ADR-094-lifecycle-telemetry-events.md)

## Context

The daemon performs both request-driven work and background work such as index warmup,
checkpointing, reindexing, and maintenance. Wall-clock duration alone does not identify
which class of work consumed resources, and process-level CPU totals cannot be attributed to
a request or background phase.

The event plane already records one audit event for a dispatch and lifecycle events for
daemon phases. Resource accounting should enrich those records rather than introduce a
second durable accounting substrate.

## Decision

Adopt a single event-plane attribution model:

```text
attributed work = principal × work_class × measured resources × executed units
```

The model applies to request dispatches and bounded background phases. It is diagnostic and
operational telemetry. This ADR defines observation only; it does not define admission,
quota, payment, or priority policy.

### 1. Closed work-class taxonomy

Every attributed record carries one of four values:

| `work_class`  | Meaning                                                                                                  |
| ------------- | -------------------------------------------------------------------------------------------------------- |
| `interactive` | Synchronous work initiated by a request.                                                                 |
| `warm`        | Index, model, or cache warmup.                                                                           |
| `maintenance` | Checkpoint, reindex, backfill, prune, vacuum, or migration work.                                         |
| `inference`   | A distinct background or batch inference phase. Inline inference remains part of its initiating request. |

`interactive` is the default for request dispatch. Adding a value requires an ADR
amendment so producers and readers remain exhaustive.

### 2. Existing audit-row enrichment

The existing per-dispatch audit event gains a `resource` object in its JSON payload:

```json
{
  "resource": {
    "work_class": "interactive",
    "cpu_us": 1840000,
    "cost_unit": 12,
    "units": {
      "fts_passes": 1,
      "vector_passes": 2,
      "graph_hops": 34,
      "db_round_trips": 5,
      "event_rows": 1
    }
  }
}
```

This is payload enrichment, not a new row or table. Existing audit identity fields continue
to carry the principal, verb, namespace, outcome, and timestamp. The existing
`duration_us` field records wall-clock duration.

`cpu_us` is measured thread CPU time for work that remains on the dispatch thread.
`cost_unit` is a deterministic operation weight derived from request and result shape. It
supports stable comparisons across runs but is not a substitute for measured execution.

### 3. Executed-unit counters

The `units` object reports work that actually completed inline during the dispatch:

| Counter             | Unit                                                                        |
| ------------------- | --------------------------------------------------------------------------- |
| `embed_calls`       | One text processed by one embedding engine.                                 |
| `fts_passes`        | One full-text query execution.                                              |
| `vector_passes`     | One vector or ANN probe.                                                    |
| `graph_hops`        | One adjacency entry returned from storage before visited-set deduplication. |
| `db_round_trips`    | One batched storage call issued by the request path.                        |
| `ann_jobs_consumed` | One deferred ANN-index job consumed inline.                                 |
| `event_rows`        | One non-audit event row successfully appended before the usage snapshot.    |

Counters are non-negative integers. The vocabulary is closed; additions require an
amendment. Within a present `units` object, an omitted counter means zero.

Collection is all-or-nothing. If the accounting context is unavailable or any counter
cannot be computed reliably, the response and audit event omit `units` entirely. A
reporting failure never changes the operation's success or error result.

### 4. Dispatch accounting context

The registry creates an accounting context around each dispatch. Central runtime seams
increment it for embedding, full-text search, vector search, graph expansion, storage
round-trips, ANN maintenance, and event append.

The context covers:

- the dispatch future;
- futures directly awaited or joined by the dispatch; and
- request-owned child tasks whose handles are joined before the response is produced.

Detached tasks and any work that may outlive the response are excluded. They are attributed
through phase events. An explicitly propagated shared accumulator is required because
task-local state does not automatically cross spawned-task boundaries.

After request-owned children finish, the runtime freezes one snapshot. That exact snapshot
is used for both the response envelope and audit payload. The enclosing audit row is not
included in its own `event_rows` counter.

### 5. Response-envelope usage

Each per-operation response may contain a sibling `usage` object:

```json
{
  "ok": true,
  "tool": "context",
  "result": {},
  "usage": {
    "graph_hops": 34,
    "db_round_trips": 5
  }
}
```

Batch responses report usage per operation and do not add an aggregate. Consumers may sum
the counters. The field is additive; consumers must ignore unknown future counters.

### 6. Background phase spans

Work that is not owned by a request uses three additive event kinds:

- `PhaseStarted`
- `PhaseCompleted`
- `PhaseCancelled`

Each occurrence emits one start and one terminal event:

```json
{
  "work_class": "warm",
  "phase": "ann_warm",
  "corpus_size": 553000,
  "wall_us": 41000000,
  "cpu_us": 514000000
}
```

The start event carries known input size. The terminal event carries elapsed resource
measurements and an outcome. Emission follows ADR-094: direct, best-effort event append,
with failures logged and no request-surface verb added.

### 7. Audit-row timing

Final duration, outcome, and usage are known only after dispatch completes. The successful
or failed dispatch audit row is therefore appended after the handler returns.

A process termination during the handler can leave no completed audit row for that
dispatch. Recording a pre-dispatch row as successful with zero duration would be a false
attribution. If crash-complete accounting becomes a requirement, the event store needs a
separate begin/finalize record contract; this ADR does not introduce one.

### 8. Write-load bound

Audit enrichment adds no rows. Background telemetry is edge-triggered, not emitted per
poll or per item. This keeps additional WAL traffic bounded and compatible with the
checkpoint constraints in ADR-091.

## Failure behavior

- Counter overflow, invalid negative values, or incomplete snapshots omit `usage` and
  `resource.units`; they never wrap.
- A phase without a terminal event is interpreted as interrupted, not completed.
- Event-store write failure is logged and otherwise follows ADR-094's best-effort contract.
- Unknown `work_class` values are rejected by producers and surfaced as unknown by
  forward-compatible readers.
- Detached work must not retain a dispatch accounting context.

## Verification

Tests must cover:

- every work-class value and unknown-value handling;
- one request whose usage includes each counter seam;
- joined child-task propagation;
- detached work exclusion;
- identical response and audit snapshots;
- omission on partial or failed accounting;
- batch response isolation;
- audit rows excluded from their own `event_rows` count;
- phase start/completion and start/cancellation sequencing; and
- bounded event-row growth under repeated background polling.

## Alternatives considered

| Alternative                                | Reason rejected                                                    |
| ------------------------------------------ | ------------------------------------------------------------------ |
| A separate resource row for every dispatch | Duplicates the existing audit stream and increases WAL write load. |
| A process-local ring buffer only           | Cannot support durable post-run attribution.                       |
| Request-shape weights only                 | Stable but cannot describe work actually executed.                 |
| Operating-system counters only             | Cannot attribute shared-process work to a request or daemon phase. |

## Consequences

### Positive

- Request and background resource use share one durable event-plane model.
- Executed work becomes visible without adding tables or per-dispatch rows.
- Response and audit readers receive the same immutable usage snapshot.

### Negative

- Runtime choke points must propagate and increment a shared accounting context.
- Post-dispatch audit timing cannot describe a request interrupted by process termination.
- Best-effort accounting means consumers must distinguish an absent object from zero usage.

## References

- [ADR-016](./ADR-016-request-dsl.md): response-envelope grammar
- [ADR-091](./ADR-091-wal-snapshot-lifetime.md): WAL and checkpoint constraints
- [ADR-094](./ADR-094-lifecycle-telemetry-events.md): event emission and sequencing
