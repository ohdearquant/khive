# ADR-165: Read-Path Performance Recovery

**Status**: proposed\
**Date**: 2026-08-17\
**Authors**: khive maintainers\
**Depends on**:

- [ADR-005](ADR-005-storage-capability-traits.md) — backend-neutral storage capabilities
- [ADR-006](ADR-006-deterministic-scoring.md) — deterministic ranking and score conversion
- [ADR-012](ADR-012-retrieval-composition.md) — retrieval composition ownership
- [ADR-021](ADR-021-memory-pack.md) — memory recall contract
- [ADR-028](ADR-028-pack-scoped-backends.md) — pack-scoped backends
- [ADR-079](ADR-079-ann-persistence-warm-path-integration.md) — memory ANN warm path
  (Vamana bridge, persisted segments)
- [ADR-091](ADR-091-wal-snapshot-lifetime.md) — WAL snapshot lifetime, reader-pin
  inventory and enforcement
- [ADR-118](ADR-118-fresh-tail-recall-visibility.md) — ANN fresh-tail consistency
- [ADR-133](ADR-133-incidental-writes-off-the-request-hot-path.md) — writer-acquisition
  reduction for incidental writes (batched audit/serve appends)

**Related**:

- [ADR-166](ADR-166-hot-path-latency-regression-guard.md) — the regression guard that
  pins each slice's recovered mechanism (depends on this record, not the reverse)

**Amends**:

- [ADR-079](ADR-079-ann-persistence-warm-path-integration.md) — the warm ANN bridge
  additionally serves the `search` vector leg, not only `memory.recall`
- [ADR-091](ADR-091-wal-snapshot-lifetime.md) — its reader-pin inventory's
  "standalone connection per file-backed read" population shrinks to the pooled
  reader set plus the enumerated structural exceptions

---

## Context

Warm-path read latency has regressed by two to three orders of magnitude against the
published budget (10-15 ms per unified verb): `search` measures 0.6-5 s warm and hard-fails
at the 5 s coordinator deadline under load; `memory.recall` measures 4.9-8.6 s with a 150 s
outlier under sustained write load. A thirteen-hypothesis investigation — live-process CPU
accounting, connection-level source reads, on-disk index inspection, daemon log analysis,
and a counter-delta experiment against the serving process — established four independent
mechanisms. Each is a separate defect with its own fix; two of them close a positive
feedback loop that converts concurrency into timeouts.

### Mechanism 1 — the `search` vector leg is a full-table scan

The `search` vector leg queries sqlite-vec `vec0` virtual tables directly
(`crates/khive-db/src/stores/vectors.rs`, KNN `MATCH` query; DDL in
`crates/khive-db/src/backend.rs`). The DDL declares no partition keys, so a KNN query
linearly scans every chunk: at the current corpus (~189k vectors x 384 dims per model,
two engines) that is ~290 MB read and decoded per engine, ~580 MB per query. Repetition
appears fast only because the OS page cache absorbs the file reads; under memory
pressure the scan cost returns and scales with system load.

The memory pack already maintains warm Vamana ANN graphs over the same vector corpus
(ADR-079): both engine graphs persist beside the database, stay current under the
ADR-118 fresh-tail contract, and serve `memory.recall`'s vector leg in microseconds.
The `search` path never consults them.

### Mechanism 2 — read verbs write synchronously through a contended writer

Every `search` and `memory.recall` performs synchronous SQLite INSERTs before returning:
a per-dispatch audit row, a serve-tracking event, and (recall) per-target serve-ledger
rows. ADR-133 inventoried this population fully — in a deployed daemon **every** verb
dispatch acquires the writer at least once — and decided the fix (batched appends
sharing one transaction; per-call serve-ledger batching; classifier-driven
commit-failure handling). ADR-133 has not been implemented.

The new measurements convert ADR-133's contention argument from structural to observed:
the serving daemon's log records its own writer-begin attempts failing at the full 30 s
timeout repeatedly under ordinary concurrent load ("writer task could not begin within
30000ms because SQLite remained busy"), and a live counter-delta experiment shows one
`memory.recall` adding 1-4 writer-task acquisitions above the ambient baseline. A
nominally read-only verb therefore inherits the writer queue's worst case — this is the
dominant term in recall's latency and the mechanism behind the 150 s outlier.

### Mechanism 3 — every file-backed store read opens a fresh standalone connection

The file-backed read arm of every store (text, vectors, note, entity, graph, event)
calls `open_standalone_reader` per read. The pooled readers
(`ConnectionPool::reader`, pre-opened at init) are reached only by the non-file-backed
arm. ADR-091 documented this posture while deliberately leaving connection-acquisition
strategy untouched (its subject was transaction lifetime, not routing).

Costs, per read: connection setup and a cold SQLite page cache; across the process: a
churning population of standalone read connections whose WAL snapshots pin checkpoint
progress. The daemon log shows the consequence: sustained WAL pressure past the
high-water mark with PASSIVE checkpoints unable to reclaim. Pinned WAL grows writer
work, which lengthens the writer queue, which — via Mechanism 2 — lengthens reads. This
is the feedback loop.

### Mechanism 4 — coordinator search fans out to every backend unconditionally

The cross-backend coordinator (`crates/kkernel/src/coordinator/dispatch.rs`) fans
`search` out to all registered backends regardless of the requested `kind`. A
`kind=note` search also queries the session backend — a database two orders of
magnitude larger than any result it could contribute — paying its FTS and vector cost
on every note search. Registration knows which kinds each backend serves; the dispatch
loop discards that information.

### Non-findings (measured and excluded)

Query embedding (cached; embedding threads parked under load), dispatch-layer overhead
(sub-millisecond), per-request engine re-initialization (none), and recall's ANN leg
itself (warm, current, zero degradation warnings all day) contribute nothing
measurable. Fix effort spent on those layers is spent where the measurements show no
cost.

## Decision

A four-slice recovery program. Slice 1 is ADR-133, unchanged, elevated to first
implementation slot by the measurements above — this record adds no new decision to it.
Slices 2-4 are the decisions of this record. Each slice is independently shippable,
independently revertible, and lands together with its ADR-166 mechanism invariant so
the regression cannot silently return.

### Slice 1 — implement ADR-133 (batched incidental writes)

Implementation order within ADR-133's decisions, chosen by measured impact: D8
(acquisition-site instrumentation — partially landed via the writer-task begin
counters), D1/D1a-d (batched audit appends), D7 (per-call serve-ledger batching), D2/D3
(classifier and accounting-bearing wait), D6 (bulk read-flag form). No contract stated
here; ADR-133 is the contract.

### Slice 2 — file-backed store reads route through pooled readers

The file-backed read arm of every store routes through `ConnectionPool::reader`.
`open_standalone_reader` remains only for enumerated structural exceptions: boot-time
schema probing, diagnostics that must observe an independent snapshot
(`db_diagnostics`), and any path ADR-091 already binds to a standalone connection by
documented constraint. The exception list is closed and lives at the pool surface; a
new standalone-read call site is a reviewable event, not a default.

Contract:

- Pool capacity and checkout timeout keep their existing envs. Pool exhaustion returns
  the existing timeout error and MUST NOT fall back to a standalone open — a fallback
  would reintroduce the connection churn under exactly the load that makes it harmful,
  invisibly.
- A pooled reader returns to the pool with no open read transaction (no WAL pin across
  checkouts). This composes with ADR-091's registry: pooled reads hold their snapshot
  for the duration of one operation, bounded by the existing read-deadline context.
- Pooled vs standalone acquisition counters (already separated in `db_diagnostics`)
  make the routing observable in production; the ADR-166 invariant asserts the
  standalone counter stays flat across a read-verb suite.

### Slice 3 — `search` vector leg served by the warm ANN bridge

`search`'s vector retrieval consults the warm Vamana graphs (ADR-079) over the shared
vector corpus. The exact sqlite-vec scan remains solely as the explicit fallback for a
model with no installed graph.

Contract:

- Freshness follows the existing bridge semantics (ADR-118 fresh-tail plus
  staleness-triggered background rebuild): `search` accepts the same bounded staleness
  `memory.recall` already accepts. This is an explicit relaxation for `search` and is
  the trade this slice makes; a caller that requires scan-exact results has no flag —
  exactness at 580 MB per query is the defect this record removes, not a mode to
  preserve.
- Namespace, kind, and visibility post-filtering are identical between routes; the ANN
  route overfetches and post-filters exactly as recall does today.
- Ranking stays deterministic under the ADR-006 contract: distances convert through the
  same canonical conversion on both routes, and result ordering for equal scores keeps
  the existing tie-break.
- The serving route (ANN vs fallback) is recorded per query in `db_diagnostics`
  counters so a silent permanent fallback is detectable; the ADR-166 invariant asserts
  the fallback rate is zero on a warm store.

### Slice 4 — coordinator fan-out filtered by declared backend kinds

Backend registration declares which substrate kinds each backend serves; the
coordinator dispatch loop skips backends whose declaration cannot match the request. A
`kind=note` search no longer dispatches to the session backend.

Contract:

- The filter is a registration-time declaration, not a per-call heuristic. A backend
  that declares nothing is conservatively included (fail-open on inclusion: the filter
  can only skip a backend that affirmatively declared non-service of the kind).
- Merged-result semantics (RRF, visibility) are unchanged for the backends that remain.
- This slice is a prerequisite for any physical store split (for example a dedicated
  knowledge database): without it, each split adds one backend to every search's
  fan-out and makes latency strictly worse. With it, a split changes which backends
  serve which kinds, and fan-out cost follows the declaration.

## Consequences

- The four slices remove, respectively: writer-queue inheritance on reads (Slice 1),
  per-read connection setup plus WAL-pinning churn — which also shrinks the writer's
  own starvation, compounding Slice 1 (Slice 2), the ~580 MB-per-query scan (Slice 3),
  and cross-backend waste plus the structural obstacle to store splitting (Slice 4).
- Slice 3 makes `search` results boundedly stale in the same way recall already is.
  The staleness bound is the ADR-118 fresh-tail contract, not a new one.
- Risk concentrates in Slice 2 (connection lifecycle) and Slice 3 (ranking parity).
  Both carry route/counter observability so a production anomaly is attributable to
  the slice that caused it, and both are revertible by routing flags at their seam.
- ADR-166's guard suite is the enforcement that this recovery, unlike previous
  point fixes in this area, stays recovered: each slice's mechanism is pinned by a
  deterministic counter invariant that fails CI if the mechanism regresses.

## Verification

Each slice lands with:

1. Before/after latency for `search kind=note limit=3` and `memory.recall limit=3` on
   the same corpus and machine class, quiet and loaded, recorded in the PR.
2. Counter evidence the intended mechanism moved: Slice 1 — writer-task acquisitions
   per recall at ambient; Slice 2 — standalone acquisition counter flat under a
   read-verb suite while the pooled counter carries the traffic, WAL steady-state under
   the high-water mark; Slice 3 — ANN route counter carries the traffic, fallback near
   zero warm; Slice 4 — zero note-search dispatches to the session backend.
3. The corresponding ADR-166 mechanism invariant merged and green in the same PR.
